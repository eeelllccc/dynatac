//! L1 AT link engine: turns the modem byte stream into replies and URCs.
//!
//! A pure state machine (no I/O, no clock): the driver feeds it received
//! bytes ([`Engine::on_rx`]), commands ([`Engine::submit`]) and time
//! ([`Engine::on_tick`]), and carries out what it asks for
//! ([`Engine::poll`]): bytes to write, replies, URCs, link events.
//!
//! Behaviour (A76XX AT manual §1.5, plus the captured traces in
//! `core/tests/fixtures/modem/`):
//!   - At most one command is in flight; the rest wait in a queue.
//!   - A command's reply is its info lines and exactly one final code.
//!     Each command declares which info lines are its own ([`Info`]) and
//!     any final codes beyond `OK` / `ERROR` / `+CME ERROR` / `+CMS ERROR`
//!     ([`ReplySpec::extra_finals`]). Anything else that arrives meanwhile
//!     is a URC — e.g. `NO CARRIER` during `AT+CHUP`, which the traces
//!     show arriving *before* `CHUP`'s `OK`.
//!   - The link is only *synced* once a probe `AT` got exactly one `OK`
//!     and no further final code arrived within a quiet window. It starts
//!     unsynced, and becomes unsynced on a timeout, lost bytes during a
//!     command, a stray final code, or [`Engine::reset`]. Commands wait
//!     while unsynced; URCs keep flowing.
//!
//! Invariants (callee):
//!   - Every submitted command produces exactly one [`Output::Reply`].
//!   - At most one command (or probe) is in flight.
//!   - Every received line is a reply line, an echo of the in-flight
//!     command (dropped), or an [`Output::Urc`]. Nothing else is dropped.
//!   - A reply is never credited with a final code that arrived while the
//!     link was unsynced.
//!   - [`LinkEvent::Unsynced`] is emitted on every sync loss and
//!     [`LinkEvent::Synced`] on every recovery, so the layer above knows
//!     when information may have been missed.
//!
//! Caller invariants:
//!   - `now` never decreases between calls.
//!   - Call `on_tick` by [`Engine::next_deadline`] at the latest.
//!   - Write every [`Output::Write`] in order, before feeding later input.
//!
//! Known limit: a final code arriving later than the quiet window after
//! a resync would be credited to the next command. Command timeouts are
//! set to the manual's maximum response time, so this needs the modem to
//! break its own spec.

use std::collections::VecDeque;

use super::at::{AtEvent, AtParser};
use super::transport::Rx;

/// Milliseconds on the caller's monotonic clock.
pub type Millis = u64;

/// How long a probe `AT` may take before it is resent.
pub const PROBE_TIMEOUT_MS: Millis = 1000;
/// After a probe's final code, how long to wait for a second (stale) one
/// before trusting the link.
pub const QUIET_MS: Millis = 300;

const PROBE: &str = "AT";
const CTRL_Z: u8 = 0x1A;
const ESC: u8 = 0x1B;

/// Which received lines are a command's own info lines.
#[derive(Debug, Clone, Copy)]
pub enum Info {
    /// The reply has no info lines (`ATE0`, `AT+CHUP` …).
    None,
    /// Lines starting with one of these (`+CLCC:` …). A URC with the same
    /// prefix arriving during the command is taken as an info line; both
    /// describe the same current state.
    Prefixes(&'static [&'static str]),
    /// Lines for which the predicate holds. For replies that share a
    /// prefix with an unrelated URC: `AT+CLIP?` → `+CLIP: 1,1` versus the
    /// caller-ID URC `+CLIP: "07700900123",129`.
    Matching(fn(&str) -> bool),
    /// Any line that is not a recognised URC (`ATI`, SMS bodies …).
    AnyLine,
}

#[derive(Debug, Clone, Copy)]
pub struct ReplySpec {
    pub info: Info,
    /// Final codes besides the standard ones, e.g. `NO CARRIER` for `ATA`.
    pub extra_finals: &'static [&'static str],
}

#[derive(Debug, Clone)]
pub struct Command {
    /// Without the trailing `\r`.
    pub text: String,
    pub spec: ReplySpec,
    /// From sending to the final code (manual's "Max Response Time").
    pub timeout_ms: Millis,
    /// Sent after the modem's `> ` prompt, followed by Ctrl-Z (SMS body).
    pub payload: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub id: CommandId,
    pub outcome: Outcome,
    /// Info lines, in order. May be partial when the outcome is `Unknown`.
    pub info: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The modem answered with a failure final code.
    Failed(Failure),
    /// Sent, but no trustworthy final code: the modem may or may not have
    /// carried it out. The layer above must find out by querying.
    Unknown(UnknownReason),
    /// Never sent: the modem did not execute it.
    NotSent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Error,
    Cme(String),
    Cms(String),
    /// One of the command's `extra_finals` (`BUSY`, `NO CARRIER` …).
    Code(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    Timeout,
    BytesLost,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    /// Received bytes were lost; URCs may be missing.
    BytesLost,
    /// The link stopped being trustworthy; commands are held until `Synced`.
    Unsynced(UnsyncReason),
    /// The link is trustworthy again (also emitted on first sync).
    Synced,
    /// The modem sent `> ` when no payload was due; answered with ESC.
    UnexpectedPrompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsyncReason {
    Timeout,
    BytesLost,
    /// A final code arrived with no command in flight.
    StrayFinal,
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Write(Vec<u8>),
    Reply(Reply),
    Urc(String),
    Event(LinkEvent),
}

/// A standard final code, valid for every command.
fn standard_final(line: &str) -> Option<Outcome> {
    match line {
        "OK" => Some(Outcome::Ok),
        "ERROR" => Some(Outcome::Failed(Failure::Error)),
        _ => {
            if let Some(e) = line.strip_prefix("+CME ERROR:") {
                Some(Outcome::Failed(Failure::Cme(e.trim().to_string())))
            } else {
                line.strip_prefix("+CMS ERROR:")
                    .map(|e| Outcome::Failed(Failure::Cms(e.trim().to_string())))
            }
        }
    }
}

struct InFlight {
    id: CommandId,
    cmd: Command,
    deadline: Millis,
    info: Vec<String>,
    payload_sent: bool,
}

struct Probe {
    /// Resend if no final code by then.
    deadline: Millis,
    finals: u32,
    first_ok: bool,
    /// Set by the first final code: trust the link if nothing else
    /// arrives before this.
    settle_until: Option<Millis>,
}

enum State {
    /// Modem unpowered: nothing is written.
    Off,
    Syncing(Probe),
    Ready(Option<InFlight>),
}

pub struct Engine {
    state: State,
    parser: AtParser,
    queue: VecDeque<(CommandId, Command)>,
    out: VecDeque<Output>,
    next_id: u64,
    is_urc: fn(&str) -> bool,
}

impl Engine {
    /// A new engine, unsynced: it starts probing immediately. `is_urc`
    /// recognises URC lines (needed to tell them from `Info::AnyLine`
    /// reply lines).
    pub fn new(is_urc: fn(&str) -> bool, now: Millis) -> Self {
        let mut e = Self::off(is_urc);
        e.start_probe(now);
        e
    }

    /// A new engine for an unpowered modem: writes nothing until `reset`.
    pub fn off(is_urc: fn(&str) -> bool) -> Self {
        Self {
            state: State::Off,
            parser: AtParser::new(),
            queue: VecDeque::new(),
            out: VecDeque::new(),
            next_id: 0,
            is_urc,
        }
    }

    /// Queue a command. Its reply comes later as exactly one `Output::Reply`
    /// (immediately `NotSent` if the modem is off).
    pub fn submit(&mut self, cmd: Command, now: Millis) -> CommandId {
        let id = CommandId(self.next_id);
        self.next_id += 1;
        if matches!(self.state, State::Off) {
            self.reply(id, Outcome::NotSent, Vec::new());
        } else {
            self.queue.push_back((id, cmd));
            self.pump(now);
        }
        id
    }

    /// Feed received bytes (or a loss) from the transport.
    pub fn on_rx(&mut self, rx: Rx, now: Millis) {
        match rx {
            Rx::Data(bytes) => {
                for event in self.parser.feed(&bytes) {
                    match event {
                        AtEvent::Line(line) => self.on_line(line, now),
                        AtEvent::Prompt => self.on_prompt(),
                    }
                }
            }
            Rx::Lost => self.on_lost(now),
        }
        self.on_tick(now);
    }

    /// Advance time: timeouts, probe resends, settling.
    pub fn on_tick(&mut self, now: Millis) {
        match &mut self.state {
            State::Off => {}
            State::Ready(Some(f)) if now >= f.deadline => {
                self.unsync(UnsyncReason::Timeout, Some(UnknownReason::Timeout), now);
            }
            State::Ready(_) => {}
            State::Syncing(p) => match p.settle_until {
                Some(t) if now >= t => {
                    if p.finals == 1 && p.first_ok {
                        self.state = State::Ready(None);
                        self.out.push_back(Output::Event(LinkEvent::Synced));
                        self.pump(now);
                    } else {
                        self.start_probe(now);
                    }
                }
                Some(_) => {}
                None if now >= p.deadline => self.start_probe(now),
                None => {}
            },
        }
    }

    /// The modem (re)started or was powered on: whatever was in flight is
    /// lost, settings are gone, resync from scratch. Queued commands stay
    /// queued (the layer above reconfigures first if it needs to, by
    /// submitting before them — see `cancel_queued`).
    pub fn reset(&mut self, now: Millis) {
        self.parser.reset();
        self.unsync(UnsyncReason::Reset, Some(UnknownReason::Reset), now);
    }

    /// The modem is being powered off: fail everything, write nothing
    /// until `reset`.
    pub fn power_off(&mut self) {
        if let State::Ready(Some(f)) = std::mem::replace(&mut self.state, State::Off) {
            self.reply(f.id, Outcome::Unknown(UnknownReason::Reset), f.info);
        }
        self.cancel_queued();
        self.parser.reset();
    }

    /// Reply `NotSent` to every queued (not yet sent) command.
    pub fn cancel_queued(&mut self) {
        for (id, _) in std::mem::take(&mut self.queue) {
            self.reply(id, Outcome::NotSent, Vec::new());
        }
    }

    /// The next output to carry out, in order.
    pub fn poll(&mut self) -> Option<Output> {
        self.out.pop_front()
    }

    /// When `on_tick` must next be called, if anything is pending.
    pub fn next_deadline(&self) -> Option<Millis> {
        match &self.state {
            State::Off | State::Ready(None) => None,
            State::Ready(Some(f)) => Some(f.deadline),
            State::Syncing(p) => Some(p.settle_until.unwrap_or(p.deadline)),
        }
    }

    pub fn is_synced(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    // --- internals --------------------------------------------------------

    fn reply(&mut self, id: CommandId, outcome: Outcome, info: Vec<String>) {
        self.out.push_back(Output::Reply(Reply { id, outcome, info }));
    }

    fn write(&mut self, bytes: Vec<u8>) {
        self.out.push_back(Output::Write(bytes));
    }

    fn write_command(&mut self, text: &str) {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(b'\r');
        self.write(bytes);
    }

    fn start_probe(&mut self, now: Millis) {
        self.write_command(PROBE);
        self.state = State::Syncing(Probe {
            deadline: now + PROBE_TIMEOUT_MS,
            finals: 0,
            first_ok: false,
            settle_until: None,
        });
    }

    /// Lose sync: settle the in-flight command (if any) as `Unknown`, then
    /// probe. Does nothing to an `Off` engine except for `Reset`, which
    /// is how it is switched back on.
    fn unsync(&mut self, reason: UnsyncReason, unknown: Option<UnknownReason>, now: Millis) {
        match std::mem::replace(&mut self.state, State::Off) {
            State::Off if reason != UnsyncReason::Reset => return,
            State::Ready(Some(f)) => {
                let why = unknown.unwrap_or(UnknownReason::Reset);
                self.reply(f.id, Outcome::Unknown(why), f.info);
            }
            _ => {}
        }
        self.out.push_back(Output::Event(LinkEvent::Unsynced(reason)));
        self.start_probe(now);
    }

    fn pump(&mut self, now: Millis) {
        if !matches!(self.state, State::Ready(None)) {
            return;
        }
        let Some((id, cmd)) = self.queue.pop_front() else { return };
        self.write_command(&cmd.text);
        let deadline = now + cmd.timeout_ms;
        self.state = State::Ready(Some(InFlight { id, cmd, deadline, info: Vec::new(), payload_sent: false }));
    }

    fn on_line(&mut self, line: String, now: Millis) {
        let is_urc = self.is_urc;
        match &mut self.state {
            State::Off => self.out.push_back(Output::Urc(line)),
            State::Syncing(p) => {
                if line == PROBE {
                    return; // echo of the probe
                }
                if standard_final(&line).is_none() {
                    self.out.push_back(Output::Urc(line));
                    return;
                }
                p.finals += 1;
                if p.finals == 1 {
                    p.first_ok = line == "OK";
                    p.settle_until = Some(now + QUIET_MS);
                } else {
                    // Two finals for one probe: one was stale. Try again.
                    self.start_probe(now);
                }
            }
            State::Ready(None) => {
                if standard_final(&line).is_some() {
                    self.unsync(UnsyncReason::StrayFinal, None, now);
                } else {
                    self.out.push_back(Output::Urc(line));
                }
            }
            State::Ready(Some(f)) => {
                if line == f.cmd.text {
                    return; // echo (echo is on until ATE0, e.g. after a restart)
                }
                let outcome = standard_final(&line).or_else(|| {
                    f.cmd.spec.extra_finals.contains(&line.as_str())
                        .then(|| Outcome::Failed(Failure::Code(line.clone())))
                });
                if let Some(outcome) = outcome {
                    let State::Ready(Some(f)) = std::mem::replace(&mut self.state, State::Ready(None)) else {
                        unreachable!()
                    };
                    self.reply(f.id, outcome, f.info);
                    self.pump(now);
                    return;
                }
                let own = match f.cmd.spec.info {
                    Info::None => false,
                    Info::Prefixes(ps) => ps.iter().any(|p| line.starts_with(p)),
                    Info::Matching(f) => f(&line),
                    Info::AnyLine => !is_urc(&line),
                };
                if own {
                    f.info.push(line);
                } else {
                    self.out.push_back(Output::Urc(line));
                }
            }
        }
    }

    fn on_prompt(&mut self) {
        if let State::Ready(Some(f)) = &mut self.state {
            if let (Some(payload), false) = (&f.cmd.payload, f.payload_sent) {
                let mut bytes = payload.clone();
                bytes.push(CTRL_Z);
                f.payload_sent = true;
                self.write(bytes);
                return;
            }
        }
        // Nothing to send: cancel the modem's input mode.
        self.write(vec![ESC]);
        self.out.push_back(Output::Event(LinkEvent::UnexpectedPrompt));
    }

    fn on_lost(&mut self, now: Millis) {
        // The partial line on either side of the gap is corrupt.
        self.parser.reset();
        self.out.push_back(Output::Event(LinkEvent::BytesLost));
        match &self.state {
            State::Off | State::Ready(None) => {} // no reply was due: only URCs lost
            State::Ready(Some(_)) => {
                self.unsync(UnsyncReason::BytesLost, Some(UnknownReason::BytesLost), now)
            }
            State::Syncing(_) => self.start_probe(now),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: ReplySpec = ReplySpec { info: Info::None, extra_finals: &[] };
    const CLCC: ReplySpec = ReplySpec { info: Info::Prefixes(&["+CLCC:"]), extra_finals: &[] };
    const ANY: ReplySpec = ReplySpec { info: Info::AnyLine, extra_finals: &[] };
    const ATA: ReplySpec = ReplySpec { info: Info::None, extra_finals: &["NO CARRIER"] };

    fn is_urc(line: &str) -> bool {
        ["RING", "NO CARRIER", "+CLCC:", "+CLIP:", "VOICE CALL:", "+CGEV:", "*ATREADY:"]
            .iter()
            .any(|p| line.starts_with(p))
    }

    fn cmd(text: &str, spec: ReplySpec) -> Command {
        Command { text: text.into(), spec, timeout_ms: 5000, payload: None }
    }

    /// Test harness: an engine plus a clock, collecting outputs.
    struct Rig {
        e: Engine,
        now: Millis,
    }

    impl Rig {
        /// Synced at t = 0 … QUIET_MS, outputs drained.
        fn synced() -> Self {
            let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
            r.rx("\r\nOK\r\n");
            r.advance(QUIET_MS);
            let out = r.take();
            assert_eq!(out, [Output::Write(b"AT\r".to_vec()), Output::Event(LinkEvent::Synced)]);
            r
        }

        fn rx(&mut self, s: &str) {
            self.e.on_rx(Rx::Data(s.as_bytes().to_vec()), self.now);
        }

        fn lost(&mut self) {
            self.e.on_rx(Rx::Lost, self.now);
        }

        fn advance(&mut self, ms: Millis) {
            self.now += ms;
            self.e.on_tick(self.now);
        }

        fn submit(&mut self, c: Command) -> CommandId {
            self.e.submit(c, self.now)
        }

        fn take(&mut self) -> Vec<Output> {
            std::iter::from_fn(|| self.e.poll()).collect()
        }
    }

    fn write(s: &str) -> Output {
        Output::Write(s.as_bytes().to_vec())
    }

    fn urc(s: &str) -> Output {
        Output::Urc(s.into())
    }

    fn reply(id: CommandId, outcome: Outcome, info: &[&str]) -> Output {
        Output::Reply(Reply { id, outcome, info: info.iter().map(|s| s.to_string()).collect() })
    }

    fn ev(e: LinkEvent) -> Output {
        Output::Event(e)
    }

    // --- sync ---------------------------------------------------------------

    #[test]
    fn starts_unsynced_and_probes() {
        let mut e = Engine::new(is_urc, 0);
        assert!(!e.is_synced());
        assert_eq!(e.poll(), Some(write("AT\r")));
        assert_eq!(e.next_deadline(), Some(PROBE_TIMEOUT_MS));
    }

    #[test]
    fn probe_ok_then_quiet_window_syncs() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.rx("\r\nOK\r\n");
        r.advance(QUIET_MS - 1);
        assert!(!r.e.is_synced());
        r.advance(1);
        assert!(r.e.is_synced());
        assert_eq!(r.take(), [ev(LinkEvent::Synced)]);
    }

    #[test]
    fn unanswered_probe_is_resent() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.advance(PROBE_TIMEOUT_MS);
        assert_eq!(r.take(), [write("AT\r")]);
    }

    #[test]
    fn probe_echo_is_ignored() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.rx("AT\r\r\nOK\r\n");
        r.advance(QUIET_MS);
        assert_eq!(r.take(), [ev(LinkEvent::Synced)]);
    }

    /// A stale final from a timed-out command lands in the probe window:
    /// two finals for one probe, so nothing is trusted and it probes again.
    #[test]
    fn second_final_during_settling_restarts_the_probe() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.rx("\r\nOK\r\n");
        r.advance(100);
        r.rx("\r\nOK\r\n");
        assert_eq!(r.take(), [write("AT\r")]);
        r.rx("\r\nOK\r\n");
        r.advance(QUIET_MS);
        assert_eq!(r.take(), [ev(LinkEvent::Synced)]);
    }

    #[test]
    fn probe_error_does_not_sync() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.rx("\r\nERROR\r\n");
        r.advance(QUIET_MS);
        assert!(!r.e.is_synced());
        assert_eq!(r.take(), [write("AT\r")]);
    }

    #[test]
    fn urcs_flow_while_unsynced() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.rx("\r\n*ATREADY: 1\r\n\r\nRING\r\n");
        assert_eq!(r.take(), [urc("*ATREADY: 1"), urc("RING")]);
    }

    #[test]
    fn commands_wait_until_synced() {
        let mut r = Rig { e: Engine::new(is_urc, 0), now: 0 };
        r.take();
        r.submit(cmd("ATE0", NONE));
        assert_eq!(r.take(), []);
        r.rx("\r\nOK\r\n");
        r.advance(QUIET_MS);
        assert_eq!(r.take(), [ev(LinkEvent::Synced), write("ATE0\r")]);
    }

    // --- replies ------------------------------------------------------------

    #[test]
    fn simple_command_ok() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("ATE0", NONE));
        r.rx("ATE0\r\r\nOK\r\n"); // echoed, as captured
        assert_eq!(r.take(), [write("ATE0\r"), reply(id, Outcome::Ok, &[])]);
    }

    #[test]
    fn info_lines_by_prefix() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("AT+CLCC", CLCC));
        r.rx("\r\n+CLCC: 1,0,0,0,0,\"07700900123\",129,\"\"\r\n\r\nOK\r\n");
        assert_eq!(
            r.take(),
            [
                write("AT+CLCC\r"),
                reply(id, Outcome::Ok, &["+CLCC: 1,0,0,0,0,\"07700900123\",129,\"\""])
            ]
        );
    }

    #[test]
    fn any_line_info_keeps_urcs_apart() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("ATI", ANY));
        r.rx("\r\nModel: A7682E\r\n\r\nRING\r\n+GCAP: +DS\r\n\r\nOK\r\n");
        assert_eq!(
            r.take(),
            [write("ATI\r"), urc("RING"), reply(id, Outcome::Ok, &["Model: A7682E", "+GCAP: +DS"])]
        );
    }

    #[test]
    fn info_lines_by_predicate_leave_same_prefix_urcs_alone() {
        fn clip_setting(l: &str) -> bool {
            l.strip_prefix("+CLIP: ").is_some_and(|r| r.split(',').all(|f| f.parse::<u8>().is_ok()))
        }
        let spec = ReplySpec { info: Info::Matching(clip_setting), extra_finals: &[] };
        let mut r = Rig::synced();
        let id = r.submit(cmd("AT+CLIP?", spec));
        r.rx("\r\n+CLIP: \"07700900123\",129\r\n\r\n+CLIP: 1,1\r\n\r\nOK\r\n");
        assert_eq!(
            r.take(),
            [write("AT+CLIP?\r"), urc("+CLIP: \"07700900123\",129"), reply(id, Outcome::Ok, &["+CLIP: 1,1"])]
        );
    }

    #[test]
    fn off_engine_writes_nothing_until_reset() {
        let mut e = Engine::off(is_urc);
        assert_eq!(e.poll(), None);
        assert_eq!(e.next_deadline(), None);
        e.reset(0);
        assert_eq!(e.poll(), Some(ev(LinkEvent::Unsynced(UnsyncReason::Reset))));
        assert_eq!(e.poll(), Some(write("AT\r")));
    }

    #[test]
    fn error_finals() {
        let mut r = Rig::synced();
        let a = r.submit(cmd("AT+CLCC=1", NONE));
        r.rx("\r\nERROR\r\n");
        let b = r.submit(cmd("AT+CPIN?", NONE));
        r.rx("\r\n+CME ERROR: SIM not inserted\r\n");
        let c = r.submit(cmd("AT+CMGS", NONE));
        r.rx("\r\n+CMS ERROR: 500\r\n");
        let replies: Vec<_> = r.take().into_iter().filter(|o| matches!(o, Output::Reply(_))).collect();
        assert_eq!(
            replies,
            [
                reply(a, Outcome::Failed(Failure::Error), &[]),
                reply(b, Outcome::Failed(Failure::Cme("SIM not inserted".into())), &[]),
                reply(c, Outcome::Failed(Failure::Cms("500".into())), &[]),
            ]
        );
    }

    /// Captured (`hang_up_ok_arrives_after_no_carrier`): CHUP's OK comes
    /// after the call-ended URCs. NO CARRIER is not CHUP's final code.
    #[test]
    fn no_carrier_during_hang_up_is_a_urc() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("AT+CHUP", NONE));
        r.rx("\r\n+CGEV: NW DEACT 8,10\r\n");
        r.rx("\r\n+CLCC: 1,0,6,0,0,\"07700900123\",129,\"\"\r\n");
        r.rx("\r\nVOICE CALL: END: 000011\r\n\r\nNO CARRIER\r\n");
        r.rx("\r\nOK\r\n");
        assert_eq!(
            r.take(),
            [
                write("AT+CHUP\r"),
                urc("+CGEV: NW DEACT 8,10"),
                urc("+CLCC: 1,0,6,0,0,\"07700900123\",129,\"\""),
                urc("VOICE CALL: END: 000011"),
                urc("NO CARRIER"),
                reply(id, Outcome::Ok, &[]),
            ]
        );
    }

    #[test]
    fn extra_final_ends_its_command() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("ATA", ATA));
        r.rx("\r\nNO CARRIER\r\n");
        assert_eq!(r.take(), [write("ATA\r"), reply(id, Outcome::Failed(Failure::Code("NO CARRIER".into())), &[])]);
    }

    #[test]
    fn commands_are_sent_one_at_a_time() {
        let mut r = Rig::synced();
        let a = r.submit(cmd("ATE0", NONE));
        let b = r.submit(cmd("AT+CMEE=2", NONE));
        assert_eq!(r.take(), [write("ATE0\r")]);
        r.rx("\r\nOK\r\n");
        assert_eq!(r.take(), [reply(a, Outcome::Ok, &[]), write("AT+CMEE=2\r")]);
        r.rx("\r\nOK\r\n");
        assert_eq!(r.take(), [reply(b, Outcome::Ok, &[])]);
    }

    #[test]
    fn urc_while_idle() {
        let mut r = Rig::synced();
        r.rx("\r\nNO CARRIER\r\n\r\nRING\r\n");
        assert_eq!(r.take(), [urc("NO CARRIER"), urc("RING")]);
        assert!(r.e.is_synced());
    }

    // --- prompt ---------------------------------------------------------------

    #[test]
    fn payload_is_sent_on_prompt() {
        let mut r = Rig::synced();
        let id = r.submit(Command {
            text: "AT+CMGS=\"07700900123\"".into(),
            spec: ReplySpec { info: Info::Prefixes(&["+CMGS:"]), extra_finals: &[] },
            timeout_ms: 60_000,
            payload: Some(b"hi".to_vec()),
        });
        r.rx("\r\n> ");
        r.rx("\r\n+CMGS: 7\r\n\r\nOK\r\n");
        assert_eq!(
            r.take(),
            [
                write("AT+CMGS=\"07700900123\"\r"),
                Output::Write(b"hi\x1A".to_vec()),
                reply(id, Outcome::Ok, &["+CMGS: 7"]),
            ]
        );
    }

    #[test]
    fn unexpected_prompt_is_cancelled() {
        let mut r = Rig::synced();
        r.rx("\r\n> ");
        assert_eq!(r.take(), [Output::Write(vec![ESC]), ev(LinkEvent::UnexpectedPrompt)]);
    }

    // --- faults ---------------------------------------------------------------

    #[test]
    fn timeout_gives_unknown_and_resyncs() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("AT+COPS?", NONE));
        r.take();
        r.advance(5000);
        assert_eq!(
            r.take(),
            [
                reply(id, Outcome::Unknown(UnknownReason::Timeout), &[]),
                ev(LinkEvent::Unsynced(UnsyncReason::Timeout)),
                write("AT\r"),
            ]
        );
        // The late OK lands in the probe window, then the probe's own OK:
        // two finals, so probe again rather than trust either.
        r.rx("\r\nOK\r\n");
        r.advance(10);
        r.rx("\r\nOK\r\n");
        assert_eq!(r.take(), [write("AT\r")]);
        r.rx("\r\nOK\r\n");
        r.advance(QUIET_MS);
        assert_eq!(r.take(), [ev(LinkEvent::Synced)]);
    }

    #[test]
    fn late_final_is_never_credited_to_the_next_command() {
        let mut r = Rig::synced();
        let slow = r.submit(cmd("AT+COPS?", NONE));
        let next = r.submit(cmd("AT+CLCC", CLCC));
        r.advance(5000); // slow times out
        r.rx("\r\nOK\r\n"); // its late OK …
        r.rx("\r\nOK\r\n"); // … and the probe's
        r.rx("\r\nOK\r\n"); // second probe
        r.advance(QUIET_MS);
        r.rx("\r\nOK\r\n"); // CLCC's real reply
        let replies: Vec<_> = r.take().into_iter().filter(|o| matches!(o, Output::Reply(_))).collect();
        assert_eq!(
            replies,
            [reply(slow, Outcome::Unknown(UnknownReason::Timeout), &[]), reply(next, Outcome::Ok, &[])]
        );
    }

    #[test]
    fn bytes_lost_mid_command_gives_unknown_and_resyncs() {
        let mut r = Rig::synced();
        let id = r.submit(cmd("AT+CLCC", CLCC));
        r.rx("\r\n+CLCC: 1,0,0");
        r.lost();
        assert_eq!(
            r.take(),
            [
                write("AT+CLCC\r"),
                ev(LinkEvent::BytesLost),
                reply(id, Outcome::Unknown(UnknownReason::BytesLost), &[]),
                ev(LinkEvent::Unsynced(UnsyncReason::BytesLost)),
                write("AT\r"),
            ]
        );
    }

    #[test]
    fn bytes_lost_while_idle_keeps_sync() {
        let mut r = Rig::synced();
        r.rx("\r\nRI");
        r.lost();
        r.rx("NG\r\n\r\nNO CARRIER\r\n");
        // The fragment after the gap surfaces as a URC; the layer above
        // treats lines after BytesLost as suspect and reconciles.
        assert_eq!(r.take(), [ev(LinkEvent::BytesLost), urc("NG"), urc("NO CARRIER")]);
        assert!(r.e.is_synced());
    }

    #[test]
    fn stray_final_while_idle_resyncs() {
        let mut r = Rig::synced();
        r.rx("\r\nOK\r\n");
        assert_eq!(r.take(), [ev(LinkEvent::Unsynced(UnsyncReason::StrayFinal)), write("AT\r")]);
    }

    #[test]
    fn reset_settles_in_flight_and_keeps_queue() {
        let mut r = Rig::synced();
        let a = r.submit(cmd("AT+CLCC", CLCC));
        let b = r.submit(cmd("ATE0", NONE));
        r.take();
        r.e.reset(r.now);
        assert_eq!(
            r.take(),
            [
                reply(a, Outcome::Unknown(UnknownReason::Reset), &[]),
                ev(LinkEvent::Unsynced(UnsyncReason::Reset)),
                write("AT\r"),
            ]
        );
        r.rx("\r\nOK\r\n");
        r.advance(QUIET_MS);
        assert_eq!(r.take(), [ev(LinkEvent::Synced), write("ATE0\r")]);
        r.rx("\r\nOK\r\n");
        assert_eq!(r.take(), [reply(b, Outcome::Ok, &[])]);
    }

    #[test]
    fn power_off_fails_everything_and_writes_nothing() {
        let mut r = Rig::synced();
        let a = r.submit(cmd("AT+CLCC", CLCC));
        let b = r.submit(cmd("ATE0", NONE));
        r.take();
        r.e.power_off();
        let c = r.submit(cmd("ATI", ANY));
        r.advance(10_000);
        assert_eq!(
            r.take(),
            [
                reply(a, Outcome::Unknown(UnknownReason::Reset), &[]),
                reply(b, Outcome::NotSent, &[]),
                reply(c, Outcome::NotSent, &[]),
            ]
        );
        assert_eq!(r.e.next_deadline(), None);
        r.e.reset(r.now);
        assert_eq!(r.take(), [ev(LinkEvent::Unsynced(UnsyncReason::Reset)), write("AT\r")]);
    }

    /// The captured ATD race: the bearer URC sometimes lands inside the
    /// reply. Every split of the byte stream gives the same outputs.
    #[test]
    fn urc_inside_reply_at_every_split_point() {
        let stream = b"\r\n+CGEV: NW ACT 8,10\r\n\r\nOK\r\n\r\n+CLCC: 1,0,2,0,0,\"07700900123\",129,\"\"\r\n";
        let dial = ReplySpec { info: Info::None, extra_finals: &["BUSY", "NO ANSWER", "NO CARRIER", "NO DIALTONE"] };
        let expected = |id| {
            vec![
                write("ATD07700900123;\r"),
                urc("+CGEV: NW ACT 8,10"),
                reply(id, Outcome::Ok, &[]),
                urc("+CLCC: 1,0,2,0,0,\"07700900123\",129,\"\""),
            ]
        };
        for split in 0..=stream.len() {
            let mut r = Rig::synced();
            let id = r.submit(cmd("ATD07700900123;", dial));
            r.e.on_rx(Rx::Data(stream[..split].to_vec()), r.now);
            r.e.on_rx(Rx::Data(stream[split..].to_vec()), r.now);
            assert_eq!(r.take(), expected(id), "split at {split}");
        }
    }
}
