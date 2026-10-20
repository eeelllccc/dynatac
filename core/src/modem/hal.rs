//! L2 modem HAL: typed requests in, typed responses and events out.
//!
//! A pure state machine on top of the L1 link engine ([`super::link`]),
//! driven the same way: feed it time, received bytes and requests; carry
//! out its outputs ([`Hal::poll`]).
//!
//! Lifecycle:
//!   - Starts `Off`: writes nothing (an unpowered modem must not be driven).
//!   - [`Hal::power_on`] → the link syncs → the HAL applies [`config_steps`]
//!     and reads each setting back → [`Event::Configured`] → requests flow.
//!   - Configuration is re-applied after *every* sync, not just power-on:
//!     after lost bytes or a timeout the modem may have restarted unseen,
//!     and every setting is idempotent. So requests never run against a
//!     modem in its default configuration (e.g. `ATH` ignored for voice).
//!   - `*ATREADY: 1` means the modem rebooted: [`Event::Restarted`], the
//!     link is reset (anything in flight settles as `Unknown`), then
//!     resync and reconfigure as above.
//!   - Configuration usually runs mid-boot (~1 s after `*ATREADY`), before
//!     the SIM is ready, and SIM-dependent settings (`AT+CLIP`) answer
//!     `ERROR` then (seen on hardware). So if a pass had issues, it is
//!     re-run at each boot milestone (`+CPIN: READY`, `SMS DONE`,
//!     `PB DONE`), each followed by a fresh `Configured`. Each milestone
//!     arrives once per boot, so re-runs are bounded.
//!
//! Invariants (callee):
//!   - Every submitted request gets exactly one [`Output::Done`].
//!   - Requests are sent in submission order, one at a time, and only
//!     while configured.
//!   - Every received line surfaces: as part of a response, or as an
//!     [`Event::Urc`] (unknown lines as `Unsolicited::Unrecognised`).
//!   - After [`Event::LinkLost`], [`Event::BytesLost`] or
//!     [`Event::Restarted`], state may have been missed; the layer above
//!     should re-query what it cares about once [`Event::Configured`]
//!     arrives (queries are truth, URCs are hints).
//!
//! Caller invariants: as for [`super::link::Engine`].

use std::collections::VecDeque;

use super::link::{self, CommandId, Engine, Failure, LinkEvent, Millis, UnknownReason, UnsyncReason};
use super::request::{Gain, Request, Response};
use super::transport::Rx;
use super::unsolicited::{self, Unsolicited};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The modem refused it (`ERROR`, `+CME ERROR` …, or e.g. `BUSY`).
    Failed(Failure),
    /// Sent, outcome unknown (timeout, lost bytes, modem restart).
    Unknown(UnknownReason),
    /// Not sent: the modem never saw it.
    NotSent,
    /// `OK`, but the reply lines weren't in the expected format.
    Malformed(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Urc(Unsolicited),
    /// The modem rebooted (`*ATREADY: 1`).
    Restarted,
    /// Settings applied and read back; requests now flow. `issues` lists
    /// any step that failed or read back wrong (requests flow regardless).
    Configured { issues: Vec<String> },
    /// The link lost sync; requests wait until `Configured`.
    LinkLost(UnsyncReason),
    /// Received bytes were lost while idle; URCs may be missing.
    BytesLost,
    UnexpectedPrompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Write(Vec<u8>),
    Done { id: RequestId, result: Result<Response, RequestError> },
    Event(Event),
}

/// Applied, in order, after every sync. A step with an expectation is a
/// read-back; a mismatch is reported in `Configured::issues`.
///
/// `AT+CLCC=1` is deliberately absent: this firmware answers `ERROR`,
/// and the captures show `+CLCC` URCs are on by default.
pub fn config_steps() -> Vec<(Request, Option<Response>)> {
    use Request as R;
    vec![
        (R::EchoOff, None),
        // The resync probe leaves a small URC delay set: clear it first.
        (R::ClearUrcDelay, None),
        (R::QueryUrcDelay, Some(Response::Setting(0))),
        (R::SetVerboseErrors, None),
        (R::QueryVerboseErrors, Some(Response::Setting(2))),
        (R::SetVoiceHangUp, None),
        (R::QueryVoiceHangUp, Some(Response::Setting(0))),
        (R::SetCallerId, None),
        (R::QueryCallerId, Some(Response::Setting(1))),
        (R::SetOutputGain(Gain::MAX), None),
        (R::QueryOutputGain, Some(Response::Gain(Gain::MAX))),
    ]
}

enum Phase {
    Off,
    Syncing,
    Configuring { step: usize, issues: Vec<String> },
    Ready,
}

enum Pending {
    Config(usize),
    User(RequestId, Request),
}

pub struct Hal {
    engine: Engine,
    phase: Phase,
    /// The last configuration pass reported issues: re-run it at the next
    /// boot milestone.
    retry_config: bool,
    /// A milestone arrived mid-pass: start again when this pass ends.
    rerun_pending: bool,
    steps: Vec<(Request, Option<Response>)>,
    queue: VecDeque<(RequestId, Request)>,
    in_flight: Option<(CommandId, Pending)>,
    out: VecDeque<Output>,
    next_id: u64,
}

impl Hal {
    /// A HAL for an unpowered modem.
    pub fn new() -> Self {
        Self {
            engine: Engine::off(unsolicited::is_urc),
            phase: Phase::Off,
            retry_config: false,
            rerun_pending: false,
            steps: config_steps(),
            queue: VecDeque::new(),
            in_flight: None,
            out: VecDeque::new(),
            next_id: 0,
        }
    }

    /// The modem has just been powered on (or is about to answer): sync,
    /// configure, then serve requests.
    pub fn power_on(&mut self, now: Millis) {
        self.phase = Phase::Syncing;
        self.engine.reset(now);
        self.run(now);
    }

    /// The modem is being powered off: everything pending fails, nothing
    /// is written until `power_on`.
    pub fn power_off(&mut self, now: Millis) {
        self.engine.power_off();
        self.phase = Phase::Off;
        self.run(now);
        for (id, _) in std::mem::take(&mut self.queue) {
            self.done(id, Err(RequestError::NotSent));
        }
    }

    pub fn submit(&mut self, request: Request, now: Millis) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        if matches!(self.phase, Phase::Off) {
            self.done(id, Err(RequestError::NotSent));
        } else {
            self.queue.push_back((id, request));
            self.run(now);
        }
        id
    }

    pub fn on_rx(&mut self, rx: Rx, now: Millis) {
        self.engine.on_rx(rx, now);
        self.run(now);
    }

    pub fn on_tick(&mut self, now: Millis) {
        self.engine.on_tick(now);
        self.run(now);
    }

    pub fn poll(&mut self) -> Option<Output> {
        self.out.pop_front()
    }

    pub fn next_deadline(&self) -> Option<Millis> {
        self.engine.next_deadline()
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.phase, Phase::Ready)
    }

    // --- internals --------------------------------------------------------

    fn done(&mut self, id: RequestId, result: Result<Response, RequestError>) {
        self.out.push_back(Output::Done { id, result });
    }

    fn event(&mut self, e: Event) {
        self.out.push_back(Output::Event(e));
    }

    /// Translate engine outputs and send the next command, until quiet.
    fn run(&mut self, now: Millis) {
        loop {
            let mut progressed = false;
            while let Some(o) = self.engine.poll() {
                progressed = true;
                self.on_engine(o, now);
            }
            if self.pump(now) {
                progressed = true;
            }
            if !progressed {
                return;
            }
        }
    }

    fn on_engine(&mut self, o: link::Output, now: Millis) {
        match o {
            link::Output::Write(b) => self.out.push_back(Output::Write(b)),
            link::Output::Reply(r) => self.on_reply(r),
            link::Output::Urc(line) => match unsolicited::parse(&line) {
                Unsolicited::Ready => {
                    self.event(Event::Restarted);
                    self.phase = Phase::Syncing;
                    self.engine.reset(now);
                }
                u => {
                    let milestone = matches!(&u, Unsolicited::SmsReady | Unsolicited::PhonebookReady)
                        || u == Unsolicited::Sim("READY".into());
                    self.event(Event::Urc(u));
                    if milestone && self.retry_config {
                        match self.phase {
                            Phase::Ready => self.phase = Phase::Configuring { step: 0, issues: Vec::new() },
                            Phase::Configuring { .. } => self.rerun_pending = true,
                            Phase::Off | Phase::Syncing => {}
                        }
                    }
                }
            },
            link::Output::Event(LinkEvent::Synced) => {
                if !matches!(self.phase, Phase::Off) {
                    self.phase = Phase::Configuring { step: 0, issues: Vec::new() };
                }
            }
            link::Output::Event(LinkEvent::Unsynced(reason)) => match self.phase {
                Phase::Off => {}
                // Our own reset (power-on / restart) was already announced.
                Phase::Syncing if reason == UnsyncReason::Reset => {}
                _ => {
                    self.phase = Phase::Syncing;
                    self.event(Event::LinkLost(reason));
                }
            },
            link::Output::Event(LinkEvent::BytesLost) => self.event(Event::BytesLost),
            link::Output::Event(LinkEvent::UnexpectedPrompt) => self.event(Event::UnexpectedPrompt),
        }
    }

    fn on_reply(&mut self, r: link::Reply) {
        let Some((cid, pending)) = self.in_flight.take() else {
            debug_assert!(false, "reply with nothing in flight: {r:?}");
            return;
        };
        debug_assert_eq!(cid, r.id);
        match pending {
            Pending::User(id, request) => {
                let result = match r.outcome {
                    link::Outcome::Ok => request.parse(&r.info).map_err(RequestError::Malformed),
                    link::Outcome::Failed(f) => Err(RequestError::Failed(f)),
                    link::Outcome::Unknown(why) => Err(RequestError::Unknown(why)),
                    link::Outcome::NotSent => Err(RequestError::NotSent),
                };
                self.done(id, result);
            }
            Pending::Config(i) => {
                // Unknown / NotSent: the link is going down; configuration
                // restarts from the top after the next sync.
                let Phase::Configuring { step, issues } = &mut self.phase else { return };
                let (request, expect) = &self.steps[i];
                let text = request.command().text;
                match (r.outcome, expect) {
                    (link::Outcome::Ok, None) => {}
                    (link::Outcome::Ok, Some(want)) => match request.parse(&r.info) {
                        Ok(got) if &got == want => {}
                        Ok(got) => issues.push(format!("{text}: expected {want:?}, got {got:?}")),
                        Err(lines) => issues.push(format!("{text}: unexpected reply {lines:?}")),
                    },
                    (link::Outcome::Failed(f), _) => issues.push(format!("{text}: {f:?}")),
                    (link::Outcome::Unknown(_) | link::Outcome::NotSent, _) => return,
                }
                *step = i + 1;
            }
        }
    }

    /// Submit the next command if none is in flight. Returns whether it
    /// did anything.
    fn pump(&mut self, now: Millis) -> bool {
        if self.in_flight.is_some() {
            return false;
        }
        match &mut self.phase {
            Phase::Configuring { step, issues } => {
                if *step < self.steps.len() {
                    let i = *step;
                    let cid = self.engine.submit(self.steps[i].0.command(), now);
                    self.in_flight = Some((cid, Pending::Config(i)));
                } else {
                    let issues = std::mem::take(issues);
                    self.retry_config = !issues.is_empty();
                    if std::mem::take(&mut self.rerun_pending) && self.retry_config {
                        self.phase = Phase::Configuring { step: 0, issues: Vec::new() };
                        return true;
                    }
                    self.phase = Phase::Ready;
                    self.event(Event::Configured { issues });
                }
                true
            }
            Phase::Ready => {
                let Some((id, request)) = self.queue.pop_front() else { return false };
                let cid = self.engine.submit(request.command(), now);
                self.in_flight = Some((cid, Pending::User(id, request)));
                true
            }
            Phase::Off | Phase::Syncing => false,
        }
    }
}

impl Default for Hal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod on_sim;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::link::probe_answer;
    use crate::modem::request::PhoneNumber;
    use crate::modem::unsolicited::{CallState, Direction};

    /// What a correctly behaving A7682E answers to each config command.
    fn healthy(cmd: &str) -> &'static str {
        match cmd {
            "AT+CMEE?" => "\r\n+CMEE: 2\r\n\r\nOK\r\n",
            "AT+CVHU?" => "\r\n+CVHU: 0\r\n\r\nOK\r\n",
            "AT+CLIP?" => "\r\n+CLIP: 1,1\r\n\r\nOK\r\n",
            "AT+COUTGAIN?" => "\r\n+COUTGAIN: 7\r\n\r\nOK\r\n",
            "AT+CURCD?" => "\r\n+CURCD: 0,1\r\n\r\nOK\r\n",
            _ => "\r\nOK\r\n",
        }
    }

    struct Rig {
        hal: Hal,
        now: Millis,
        /// Writes not yet answered.
        writes: VecDeque<String>,
        dones: Vec<(RequestId, Result<Response, RequestError>)>,
        events: Vec<Event>,
    }

    impl Rig {
        fn new() -> Self {
            Rig { hal: Hal::new(), now: 0, writes: VecDeque::new(), dones: vec![], events: vec![] }
        }

        /// Powered on, synced and configured with a healthy modem.
        fn ready() -> Self {
            let mut r = Rig::new();
            r.power_on();
            r.collect();
            r.answer_probe();
            r.configure_with(healthy);
            assert_eq!(r.events, [Event::Configured { issues: vec![] }]);
            r.events.clear();
            r
        }

        /// Answer each config command with `respond` until configured.
        fn configure_with(&mut self, respond: fn(&str) -> &'static str) {
            while !self.hal.is_ready() {
                let cmd = self.writes.front().cloned().expect("a config command was written");
                self.answer(respond(&cmd));
            }
        }

        fn collect(&mut self) {
            while let Some(o) = self.hal.poll() {
                match o {
                    Output::Write(b) => {
                        self.writes.push_back(String::from_utf8(b).unwrap().trim_end_matches('\r').into())
                    }
                    Output::Done { id, result } => self.dones.push((id, result)),
                    Output::Event(e) => self.events.push(e),
                }
            }
        }

        /// Feed modem output; marks the oldest write as answered.
        fn answer(&mut self, s: &str) {
            self.writes.pop_front();
            self.rx(s);
        }

        fn rx(&mut self, s: &str) {
            self.hal.on_rx(Rx::Data(s.as_bytes().to_vec()), self.now);
            self.collect();
        }

        fn advance(&mut self, ms: Millis) {
            self.now += ms;
            self.hal.on_tick(self.now);
            self.collect();
        }

        /// Answer the latest resync probe written (earlier, unanswered
        /// probes are superseded).
        fn answer_probe(&mut self) {
            let probe = self.writes.iter().rev().find(|w| w.starts_with("AT+CURCD=")).cloned().expect("a probe was written");
            self.writes.clear();
            self.rx(&probe_answer(&probe).unwrap());
        }

        fn power_on(&mut self) {
            self.hal.power_on(self.now);
            self.collect();
        }

        fn submit(&mut self, r: Request) -> RequestId {
            let id = self.hal.submit(r, self.now);
            self.collect();
            id
        }
    }

    #[test]
    fn off_writes_nothing_and_refuses_requests() {
        let mut r = Rig::new();
        let id = r.submit(Request::ListCalls);
        r.advance(10_000);
        assert!(r.writes.is_empty());
        assert_eq!(r.dones, [(id, Err(RequestError::NotSent))]);
    }

    #[test]
    fn power_on_syncs_then_configures_in_order() {
        let mut r = Rig::new();
        r.power_on();
        assert_eq!(r.writes.len(), 1);
        assert!(r.writes[0].starts_with("AT+CURCD="), "{:?}", r.writes);
        r.answer_probe();
        let mut sent = vec![];
        while !r.hal.is_ready() {
            let cmd = r.writes.front().cloned().unwrap();
            sent.push(cmd.clone());
            r.answer(healthy(&cmd));
        }
        assert_eq!(
            sent,
            [
                "ATE0", "AT+CURCD=0,1", "AT+CURCD?", "AT+CMEE=2", "AT+CMEE?", "AT+CVHU=0", "AT+CVHU?", "AT+CLIP=1", "AT+CLIP?",
                "AT+COUTGAIN=7", "AT+COUTGAIN?"
            ]
        );
        assert_eq!(r.events, [Event::Configured { issues: vec![] }]);
    }

    #[test]
    fn requests_wait_for_configuration() {
        let mut r = Rig::new();
        r.power_on();
        let id = r.submit(Request::ListCalls);
        r.answer_probe();
        assert_eq!(r.writes, ["ATE0"]);
        r.configure_with(healthy);
        assert_eq!(r.writes, ["AT+CLCC"]);
        r.answer("\r\nOK\r\n");
        assert_eq!(r.dones, [(id, Ok(Response::Calls(vec![])))]);
    }

    #[test]
    fn config_problems_are_reported_and_requests_still_flow() {
        fn quiet_speaker(cmd: &str) -> &'static str {
            match cmd {
                "AT+COUTGAIN=7" => "\r\nERROR\r\n",
                "AT+COUTGAIN?" => "\r\n+COUTGAIN: 4\r\n\r\nOK\r\n",
                c => healthy(c),
            }
        }
        let mut r = Rig::new();
        r.power_on();
        r.answer_probe();
        r.configure_with(quiet_speaker);
        assert_eq!(
            r.events,
            [Event::Configured {
                issues: vec![
                    "AT+COUTGAIN=7: Error".into(),
                    "AT+COUTGAIN?: expected Gain(Gain(7)), got Gain(Gain(4))".into()
                ]
            }]
        );
        assert!(r.hal.is_ready());
    }

    #[test]
    fn dial_and_call_urcs() {
        let mut r = Rig::ready();
        let id = r.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
        assert_eq!(r.writes, ["ATD07700900123;"]);
        r.answer("\r\n+CGEV: NW ACT 8,10\r\n\r\nOK\r\n");
        r.rx("\r\n+CLCC: 1,0,2,0,0,\"07700900123\",129,\"\"\r\n");
        assert_eq!(r.dones, [(id, Ok(Response::Done))]);
        let Event::Urc(Unsolicited::Call(c)) = &r.events[1] else { panic!("{:?}", r.events) };
        assert_eq!((c.direction, c.state), (Direction::Outgoing, CallState::Dialling));
        assert_eq!(r.events[0], Event::Urc(Unsolicited::PacketDomain("NW ACT 8,10".into())));
    }

    #[test]
    fn modem_failure_codes_are_errors() {
        let mut r = Rig::ready();
        let id = r.submit(Request::QueryOutputGain);
        r.answer("\r\n+CME ERROR: operation not allowed\r\n");
        assert_eq!(r.dones, [(id, Err(RequestError::Failed(Failure::Cme("operation not allowed".into()))))]);
    }

    /// A call-progress code during `ATD` (e.g. the previous call's late
    /// `NO CARRIER`) is a URC; the dial still waits for its own `OK`.
    #[test]
    fn call_progress_codes_during_dial_are_urcs() {
        let mut r = Rig::ready();
        let id = r.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
        r.rx("\r\nNO CARRIER\r\n\r\nBUSY\r\n");
        assert!(r.dones.is_empty());
        assert_eq!(r.events, [Event::Urc(Unsolicited::NoCarrier), Event::Urc(Unsolicited::Busy)]);
        r.answer("\r\nOK\r\n");
        assert_eq!(r.dones, [(id, Ok(Response::Done))]);
    }

    #[test]
    fn malformed_reply_is_reported() {
        let mut r = Rig::ready();
        let id = r.submit(Request::QueryOutputGain);
        r.answer("\r\n+COUTGAIN: lots\r\n\r\nOK\r\n");
        assert_eq!(r.dones, [(id, Err(RequestError::Malformed(vec!["+COUTGAIN: lots".into()])))]);
    }

    #[test]
    fn restart_settles_in_flight_then_reconfigures_before_queued_requests() {
        let mut r = Rig::ready();
        let a = r.submit(Request::ListCalls);
        let b = r.submit(Request::QueryOutputGain);
        r.rx("\r\n*ATREADY: 1\r\n");
        assert_eq!(r.dones, [(a, Err(RequestError::Unknown(UnknownReason::Reset)))]);
        assert_eq!(r.events, [Event::Restarted]);
        assert!(r.writes.back().is_some_and(|w| w.starts_with("AT+CURCD="))); // probing
        r.answer_probe();
        assert_eq!(r.writes.front().map(String::as_str), Some("ATE0"));
        r.configure_with(healthy);
        assert_eq!(r.writes, ["AT+COUTGAIN?"]);
        r.answer("\r\n+COUTGAIN: 7\r\n\r\nOK\r\n");
        assert_eq!(r.dones[1], (b, Ok(Response::Gain(Gain::MAX))));
    }

    #[test]
    fn timeout_loses_the_link_and_reconfigures() {
        let mut r = Rig::ready();
        let id = r.submit(Request::ListCalls);
        r.advance(9_000);
        assert_eq!(r.dones, [(id, Err(RequestError::Unknown(UnknownReason::Timeout)))]);
        assert_eq!(r.events, [Event::LinkLost(UnsyncReason::Timeout)]);
        assert!(!r.hal.is_ready());
        r.answer_probe();
        r.configure_with(healthy);
        assert_eq!(r.events.last(), Some(&Event::Configured { issues: vec![] }));
    }

    #[test]
    fn link_lost_during_configuration_restarts_it() {
        let mut r = Rig::new();
        r.power_on();
        r.answer_probe();
        r.answer("\r\nOK\r\n"); // ATE0
        r.hal.on_rx(Rx::Lost, r.now); // during AT+CMEE=2
        r.collect();
        assert_eq!(r.events, [Event::BytesLost, Event::LinkLost(UnsyncReason::BytesLost)]);
        r.answer_probe();
        assert_eq!(r.writes.front().map(String::as_str), Some("ATE0")); // from the top
    }

    #[test]
    fn urcs_are_typed_and_unknown_lines_kept() {
        let mut r = Rig::ready();
        r.rx("\r\nRING\r\n\r\n+CLIP: \"07700900123\",129\r\n\r\nsomething new\r\n");
        assert_eq!(
            r.events,
            [
                Event::Urc(Unsolicited::Ring),
                Event::Urc(Unsolicited::CallerId { number: Some("07700900123".into()) }),
                Event::Urc(Unsolicited::Unrecognised("something new".into())),
            ]
        );
    }

    #[test]
    fn caller_id_during_clip_query_stays_a_urc() {
        let mut r = Rig::new();
        r.power_on();
        r.answer_probe();
        r.configure_with(|cmd| match cmd {
            "AT+CLIP?" => "\r\n+CLIP: \"07700900123\",129\r\n\r\n+CLIP: 1,1\r\n\r\nOK\r\n",
            c => healthy(c),
        });
        assert_eq!(
            r.events,
            [
                Event::Urc(Unsolicited::CallerId { number: Some("07700900123".into()) }),
                Event::Configured { issues: vec![] }
            ]
        );
    }

    #[test]
    fn bytes_lost_while_idle_keeps_ready() {
        let mut r = Rig::ready();
        r.hal.on_rx(Rx::Lost, r.now);
        r.collect();
        assert_eq!(r.events, [Event::BytesLost]);
        assert!(r.hal.is_ready());
    }

    #[test]
    fn power_off_fails_everything() {
        let mut r = Rig::ready();
        let a = r.submit(Request::ListCalls);
        let b = r.submit(Request::HangUp);
        r.writes.clear();
        r.hal.power_off(r.now);
        r.collect();
        let c = r.submit(Request::Answer);
        r.advance(60_000);
        assert!(r.writes.is_empty());
        assert_eq!(
            r.dones,
            [
                (a, Err(RequestError::Unknown(UnknownReason::Reset))),
                (b, Err(RequestError::NotSent)),
                (c, Err(RequestError::NotSent)),
            ]
        );
    }

    /// Seen on hardware: configuration runs before the SIM is ready and
    /// `AT+CLIP` fails; it is retried at the next boot milestone.
    #[test]
    fn sim_dependent_settings_are_retried_at_boot_milestones() {
        fn sim_not_ready(cmd: &str) -> &'static str {
            match cmd {
                "AT+CLIP=1" | "AT+CLIP?" => "\r\nERROR\r\n",
                c => healthy(c),
            }
        }
        let mut r = Rig::new();
        r.power_on();
        r.answer_probe();
        r.configure_with(sim_not_ready);
        assert_eq!(
            r.events,
            [Event::Configured { issues: vec!["AT+CLIP=1: Error".into(), "AT+CLIP?: Error".into()] }]
        );
        r.events.clear();
        let id = r.submit(Request::ListCalls);
        r.answer("\r\nOK\r\n"); // requests flow meanwhile
        assert_eq!(r.dones, [(id, Ok(Response::Calls(vec![])))]);
        r.rx("\r\n+CPIN: READY\r\n");
        assert_eq!(r.writes, ["ATE0"], "re-run from the top");
        r.configure_with(healthy);
        assert_eq!(r.events, [Event::Urc(Unsolicited::Sim("READY".into())), Event::Configured { issues: vec![] }]);
        r.events.clear();
        r.rx("\r\nSMS DONE\r\n\r\nPB DONE\r\n");
        assert!(r.writes.is_empty(), "no re-run once clean");
        assert_eq!(r.events, [Event::Urc(Unsolicited::SmsReady), Event::Urc(Unsolicited::PhonebookReady)]);
    }

    #[test]
    fn milestone_during_a_pass_reruns_it_after() {
        fn sim_not_ready(cmd: &str) -> &'static str {
            match cmd {
                "AT+CLIP=1" | "AT+CLIP?" => "\r\nERROR\r\n",
                c => healthy(c),
            }
        }
        let mut r = Rig::new();
        r.power_on();
        r.answer_probe();
        r.configure_with(sim_not_ready); // first pass: issues, retry armed
        r.events.clear();
        r.rx("\r\n+CPIN: READY\r\n"); // second pass starts
        r.answer("\r\nOK\r\n"); // ATE0
        r.rx("\r\nSMS DONE\r\n"); // milestone mid-pass
        // Answer the rest of the second pass (still failing): it reruns
        // instead of reporting.
        for _ in 1..config_steps().len() {
            let cmd = r.writes.front().cloned().unwrap();
            r.answer(sim_not_ready(&cmd));
        }
        assert!(!r.events.iter().any(|e| matches!(e, Event::Configured { .. })));
        assert_eq!(r.writes, ["ATE0"]);
        r.configure_with(healthy);
        assert_eq!(r.events.last(), Some(&Event::Configured { issues: vec![] }));
    }
}
