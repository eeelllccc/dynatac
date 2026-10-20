//! Byte-level simulator of the A7682E, on a simulated clock.
//!
//! [`Sim`] behaves like the real modem on its UART: it takes the bytes we
//! write and produces the bytes the modem would send back, when it would
//! send them. [`SimTransport`] wraps it as an L0 [`Transport`] with a
//! simulated clock, so the whole stack (link engine, HAL, services) runs
//! against it deterministically and instantly.
//!
//! What is modelled, and from where:
//!   - From the captures (`core/tests/fixtures/modem/`, checked by
//!     `core/tests/sim_replay.rs`): the boot sequence and its timing,
//!     echo until `ATE0`, reply formats and latencies, the outgoing call
//!     sequence, `VOICE CALL: BEGIN` + `+CLCC` + `+COLP` on connect, the
//!     local hang-up sequence with `OK` *last*, remote hang-up, incoming
//!     calls (`RING` + `+CLIP` every 6 s, first one followed by `+CLCC`),
//!     `ATA` with `OK` last, missed calls (`VOICE CALL: END` without a
//!     duration), soft power-off (`+CGEV: ME DETACH` only), a hard cut
//!     (silence), `AT+CLCC=1` → `ERROR`.
//!   - From the manual, not captured: `NO_SAVE` settings (`ATE`, `CMEE`,
//!     `CVHU`) reset on boot while `AUTO_SAVE` ones (`CLIP`, `COUTGAIN`)
//!     persist; `ATH` ignored for voice unless `AT+CVHU=0`; `ATA` with no
//!     call → `NO CARRIER`; the `?` query reply formats.
//!   - Guesses (no source; revisit if captured): hanging up a call that
//!     was never answered ends with `VOICE CALL: END` without a duration;
//!     rejecting a ringing call with `AT+CHUP` looks like a missed call;
//!     the modem ignores input until `*ATREADY`; unknown commands →
//!     `ERROR`; `ATD` with a call already present → `ERROR`.
//!
//! The network and the far end are inputs, not modelled behaviour: dial
//! progress, the voice bearer, and the remote side answering, hanging up,
//! calling or giving up. With [`SimConfig::auto_network`] the network
//! side of an outgoing call progresses on its own (as typically
//! captured); the far end only ever acts when a test says so.
//!
//! Not yet modelled: SMS, the RI pin, data mode, fault injection (step 5
//! of `todo.md`).

use std::collections::VecDeque;

use super::link::Millis;
use super::transport::{Clock, Rx, Transport, WriteError};
use super::unsolicited::{CallState, Direction};

/// Boot output, as offsets from the end of the PWRKEY pulse (captured).
const BOOT: &[(Millis, &str)] = &[
    (6030, "*ATREADY: 1"),
    (7995, "+CPIN: READY"),
    (8580, "SMS DONE"),
    (10400, "+CGEV: EPS PDN ACT 1"),
    (18180, "PB DONE"),
];
/// Soft power-off: `ME DETACH` this long after the pulse ends (captured).
const DETACH_AFTER: Millis = 390;
const ECHO_AFTER: Millis = 10;
const RING_PERIOD: Millis = 6000;

/// Captured `ATI` reply (IMEI scrubbed, as in the fixtures).
const ATI: &[&str] = &[
    "Manufacturer: INCORPORATED",
    "Model: A7682E",
    "Revision: A7682M7_V1.11.1",
    "IMEI: 000000000000000",
    "+GCAP: +CGSM,+FCLASS,+DS",
];

#[derive(Debug, Clone, Copy)]
pub struct SimConfig {
    /// Outgoing calls progress by themselves: bearer up just after `OK`,
    /// dialling at +2.2 s, alerting at +3.3 s (typical of the captures).
    /// The far end still never answers on its own.
    pub auto_network: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self { auto_network: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Power {
    Off,
    Booting,
    On,
}

#[derive(Debug, Clone, Copy)]
struct Settings {
    echo: bool,
    cmee: u8,
    cvhu: u8,
    clip: u8,
    gain: u8,
}

impl Settings {
    /// Defaults of the `NO_SAVE` settings after boot; `AUTO_SAVE` ones
    /// (`clip`, `gain`) keep their stored values.
    fn after_boot(self) -> Self {
        Settings { echo: true, cmee: 0, cvhu: 1, ..self }
    }
}

#[derive(Debug, Clone)]
struct Call {
    direction: Direction,
    state: CallState,
    number: String,
    active_since: Option<Millis>,
    bearer: bool,
}

#[derive(Debug)]
enum Effect {
    Emit(Vec<u8>),
    Booted,
    /// The modem finished the current command and takes the next.
    Finish,
    Dialling,
    Alerting,
    Bearer,
    /// `ATA` connected.
    Connected,
    CallGone,
    Ring,
}

#[derive(Debug)]
struct Scheduled {
    at: Millis,
    seq: u64,
    effect: Effect,
}

pub struct Sim {
    cfg: SimConfig,
    power: Power,
    settings: Settings,
    line: Vec<u8>,
    commands: VecDeque<String>,
    busy: bool,
    sched: Vec<Scheduled>,
    seq: u64,
    now: Millis,
    /// Sent chunks, with the time each went out.
    rx: VecDeque<(Millis, Vec<u8>)>,
    call: Option<Call>,
}

fn lines(ls: &[&str]) -> Vec<u8> {
    ls.iter().flat_map(|l| format!("\r\n{l}\r\n").into_bytes()).collect()
}

fn hhmmss(ms: Millis) -> String {
    let s = ms / 1000;
    format!("{:02}{:02}{:02}", s / 3600, s / 60 % 60, s % 60)
}

impl Sim {
    /// A powered-off modem with factory settings.
    pub fn new(cfg: SimConfig) -> Self {
        Sim {
            cfg,
            power: Power::Off,
            settings: Settings { echo: true, cmee: 0, cvhu: 1, clip: 0, gain: 4 },
            line: Vec::new(),
            commands: VecDeque::new(),
            busy: false,
            sched: Vec::new(),
            seq: 0,
            now: 0,
            rx: VecDeque::new(),
            call: None,
        }
    }

    // --- outside world: power and network ---------------------------------

    /// The PWRKEY on-pulse ended at `now`. Ignored unless off.
    pub fn power_on(&mut self, now: Millis) {
        self.advance(now);
        if self.power != Power::Off {
            return;
        }
        self.power = Power::Booting;
        for &(after, line) in BOOT {
            if line == "*ATREADY: 1" {
                self.at(after, Effect::Booted);
            }
            self.at(after, Effect::Emit(lines(&[line])));
        }
    }

    /// The 3 s PWRKEY off-pulse ended at `now`.
    pub fn power_off(&mut self, now: Millis) {
        self.advance(now);
        if self.power == Power::Off {
            return;
        }
        self.shut_down();
        self.at(DETACH_AFTER, Effect::Emit(lines(&["+CGEV: ME DETACH"])));
    }

    /// POWER_EN cut at `now`: instant silence, nothing in transit survives.
    pub fn cut_power(&mut self, now: Millis) {
        self.advance(now);
        self.shut_down();
    }

    pub fn network_dialling(&mut self, now: Millis) {
        self.advance(now);
        self.apply(Effect::Dialling);
    }

    pub fn network_alerting(&mut self, now: Millis) {
        self.advance(now);
        self.apply(Effect::Alerting);
    }

    /// The network brought up the voice bearer (`+CGEV: NW ACT 8,10`).
    pub fn network_bearer(&mut self, now: Millis) {
        self.advance(now);
        self.apply(Effect::Bearer);
    }

    /// The far end picked up our outgoing call.
    pub fn remote_answer(&mut self, now: Millis) {
        self.advance(now);
        let Some(c) = self.call.as_mut() else { return };
        if c.direction != Direction::Outgoing || c.state == CallState::Active {
            return;
        }
        c.state = CallState::Active;
        c.active_since = Some(now);
        let clcc = clcc_line(c);
        let colp = format!("+COLP: \"{}\",{}", c.number, number_type(&c.number));
        self.at(0, Effect::Emit(lines(&["VOICE CALL: BEGIN", &clcc])));
        self.at(15, Effect::Emit(lines(&[&colp])));
    }

    /// The far end hung up (or, for a ringing incoming call, gave up).
    pub fn remote_hang_up(&mut self, now: Millis) {
        self.advance(now);
        match self.call.as_ref().map(|c| c.state) {
            None | Some(CallState::Disconnected) => {}
            Some(CallState::Incoming | CallState::Waiting) => self.missed(0),
            Some(_) => {
                self.end_call(0);
            }
        }
    }

    /// Someone calls us.
    pub fn incoming_call(&mut self, number: &str, now: Millis) {
        self.advance(now);
        if self.power != Power::On || self.call.is_some() {
            return;
        }
        let c = Call {
            direction: Direction::Incoming,
            state: CallState::Incoming,
            number: number.to_string(),
            active_since: None,
            bearer: false,
        };
        let clcc = clcc_line(&c);
        self.call = Some(c);
        self.apply(Effect::Ring);
        self.at(40, Effect::Emit(lines(&[&clcc])));
    }

    // --- UART -------------------------------------------------------------

    /// Bytes written to the modem at `now`.
    pub fn write(&mut self, bytes: &[u8], now: Millis) {
        self.advance(now);
        if self.power != Power::On {
            return; // UART not up yet / unpowered
        }
        for &b in bytes {
            if b == b'\r' {
                let cmd = String::from_utf8_lossy(&std::mem::take(&mut self.line)).into_owned();
                if cmd.is_empty() {
                    continue;
                }
                if self.settings.echo {
                    self.at(ECHO_AFTER, Effect::Emit(format!("{cmd}\r").into_bytes()));
                }
                self.commands.push_back(cmd);
            } else if b != b'\n' {
                self.line.push(b);
            }
        }
        self.next_command();
    }

    /// Run the clock forward to `now`, carrying out everything due.
    pub fn advance(&mut self, now: Millis) {
        debug_assert!(now >= self.now, "time went backwards");
        while let Some(i) = self.next_index().filter(|&i| self.sched[i].at <= now) {
            let s = self.sched.swap_remove(i);
            self.now = s.at;
            self.apply(s.effect);
        }
        self.now = now;
    }

    /// The next chunk the modem has sent (by the last `advance`).
    pub fn take_rx(&mut self) -> Option<Vec<u8>> {
        self.rx.pop_front().map(|(_, b)| b)
    }

    /// As `take_rx`, with the time the chunk went out.
    pub fn take_rx_timed(&mut self) -> Option<(Millis, Vec<u8>)> {
        self.rx.pop_front()
    }

    /// When something is next due, if anything is.
    pub fn next_event_at(&self) -> Option<Millis> {
        self.next_index().map(|i| self.sched[i].at)
    }

    pub fn now(&self) -> Millis {
        self.now
    }

    /// The speaker gain currently set (for tests).
    pub fn gain(&self) -> u8 {
        self.settings.gain
    }

    /// The current call's state, if there is a call (for tests).
    pub fn call_state(&self) -> Option<CallState> {
        self.call.as_ref().map(|c| c.state)
    }

    // --- internals --------------------------------------------------------

    fn next_index(&self) -> Option<usize> {
        (0..self.sched.len()).min_by_key(|&i| (self.sched[i].at, self.sched[i].seq))
    }

    /// Schedule `effect` at `self.now + after`.
    fn at(&mut self, after: Millis, effect: Effect) {
        self.seq += 1;
        self.sched.push(Scheduled { at: self.now + after, seq: self.seq, effect });
    }

    fn emit(&mut self, bytes: Vec<u8>) {
        self.rx.push_back((self.now, bytes));
    }

    fn shut_down(&mut self) {
        self.power = Power::Off;
        self.sched.clear();
        self.commands.clear();
        self.line.clear();
        self.busy = false;
        self.call = None;
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Emit(b) => self.emit(b),
            Effect::Booted => {
                self.power = Power::On;
                self.settings = self.settings.after_boot();
            }
            Effect::Finish => {
                self.busy = false;
                self.next_command();
            }
            Effect::Dialling | Effect::Alerting => {
                let state = if matches!(effect, Effect::Dialling) { CallState::Dialling } else { CallState::Alerting };
                if let Some(c) = self.call.as_mut().filter(|c| {
                    c.direction == Direction::Outgoing
                        && matches!(c.state, CallState::Dialling | CallState::Alerting)
                }) {
                    c.state = state;
                    let l = clcc_line(c);
                    self.emit(lines(&[&l]));
                }
            }
            Effect::Bearer => {
                if let Some(c) = self.call.as_mut().filter(|c| !c.bearer) {
                    c.bearer = true;
                    self.emit(lines(&["+CGEV: NW ACT 8,10"]));
                }
            }
            Effect::Connected => {
                if let Some(c) = self.call.as_mut() {
                    c.state = CallState::Active;
                    c.active_since = Some(self.now);
                    let l = clcc_line(c);
                    self.emit(lines(&["VOICE CALL: BEGIN", &l]));
                }
            }
            Effect::CallGone => self.call = None,
            Effect::Ring => {
                let Some(c) = self.call.as_ref().filter(|c| c.state == CallState::Incoming) else { return };
                let mut out = vec!["RING".to_string()];
                if self.settings.clip == 1 {
                    out.push(format!("+CLIP: \"{}\",{}", c.number, number_type(&c.number)));
                }
                let refs: Vec<&str> = out.iter().map(String::as_str).collect();
                self.emit(lines(&refs));
                self.at(RING_PERIOD, Effect::Ring);
            }
        }
    }

    /// Ending sequence for a call that was set up (local or remote hang-up),
    /// starting `after` ms from now. Captured offsets: bearer down, `+CLCC`
    /// stat 6 ~0.1 s later, then `VOICE CALL: END` + `NO CARRIER` 0.25 s later.
    /// Returns when the last line goes out.
    fn end_call(&mut self, after: Millis) -> Millis {
        let Some(mut c) = self.call.clone() else { return after };
        c.state = CallState::Disconnected;
        let mut t = after;
        if c.bearer {
            self.at(t, Effect::Emit(lines(&["+CGEV: NW DEACT 8,10"])));
        }
        t += 95;
        self.at(t, Effect::Emit(lines(&[&clcc_line(&c)])));
        self.at(t, Effect::CallGone);
        t += 250;
        let end = match c.active_since {
            Some(since) => format!("VOICE CALL: END: {}", hhmmss(self.now + t - since)),
            None => "VOICE CALL: END".to_string(),
        };
        self.at(t, Effect::Emit(lines(&[&end, "NO CARRIER"])));
        t
    }

    /// A ringing call ends unanswered: one burst (captured).
    fn missed(&mut self, after: Millis) {
        let Some(mut c) = self.call.clone() else { return };
        c.state = CallState::Disconnected;
        self.at(after, Effect::CallGone);
        self.at(after, Effect::Emit(lines(&[&clcc_line(&c), "VOICE CALL: END", "NO CARRIER"])));
    }

    fn next_command(&mut self) {
        if self.busy || self.power != Power::On {
            return;
        }
        let Some(cmd) = self.commands.pop_front() else { return };
        self.busy = true;
        let done = self.execute(&cmd);
        self.at(done, Effect::Finish);
    }

    /// Carry out one command: schedule its output and effects. Returns
    /// when the final code goes out (the modem is busy until then).
    fn execute(&mut self, cmd: &str) -> Millis {
        let ok = |sim: &mut Sim, after: Millis| {
            sim.at(after, Effect::Emit(lines(&["OK"])));
            after
        };
        let error = |sim: &mut Sim| {
            sim.at(20, Effect::Emit(lines(&["ERROR"])));
            20
        };
        let reply = |sim: &mut Sim, after: Millis, info: &[&str]| {
            let mut all: Vec<&str> = info.to_vec();
            all.push("OK");
            sim.at(after, Effect::Emit(lines(&all)));
            after
        };
        let upper = cmd.to_ascii_uppercase();
        let set = |prefix: &str, max: u8| -> Option<u8> {
            upper.strip_prefix(prefix)?.parse().ok().filter(|&v| v <= max)
        };

        match upper.as_str() {
            "AT" => return ok(self, 10),
            "ATE0" | "ATE1" => {
                self.settings.echo = upper == "ATE1";
                return ok(self, 10);
            }
            "AT+CMEE?" => return reply(self, 15, &[&format!("+CMEE: {}", self.settings.cmee)]),
            "AT+CVHU?" => return reply(self, 15, &[&format!("+CVHU: {}", self.settings.cvhu)]),
            "AT+CLIP?" => return reply(self, 15, &[&format!("+CLIP: {},1", self.settings.clip)]),
            "AT+COUTGAIN?" => return reply(self, 15, &[&format!("+COUTGAIN: {}", self.settings.gain)]),
            "AT+CLCC=1" => return error(self),
            "AT+CLCC" => {
                let calls: Vec<String> = self.call.iter().map(clcc_line).collect();
                let refs: Vec<&str> = calls.iter().map(String::as_str).collect();
                return reply(self, 25, &refs);
            }
            "ATI" => return reply(self, 25, ATI),
            "AT+CGMR" => return reply(self, 12, &["+CGMR: A011B15A7682M7"]),
            "ATA" => return self.answer(),
            "AT+CHUP" => return self.hang_up(),
            "ATH" => {
                // Ignored for voice calls unless AT+CVHU=0 (manual).
                return if self.settings.cvhu == 0 { self.hang_up() } else { ok(self, 15) };
            }
            _ => {}
        }
        if let Some(v) = set("AT+CMEE=", 2) {
            self.settings.cmee = v;
            return ok(self, 10);
        }
        if let Some(v) = set("AT+CVHU=", 1) {
            self.settings.cvhu = v;
            return ok(self, 10);
        }
        if let Some(v) = set("AT+CLIP=", 1) {
            self.settings.clip = v;
            return ok(self, 100);
        }
        if let Some(v) = set("AT+COUTGAIN=", 7) {
            self.settings.gain = v;
            return ok(self, 270);
        }
        if let Some(number) = upper.strip_prefix("ATD").and_then(|n| n.strip_suffix(';')) {
            return self.dial(&cmd[3..3 + number.len()]);
        }
        error(self)
    }

    fn dial(&mut self, number: &str) -> Millis {
        if self.call.is_some() || number.is_empty() {
            self.at(20, Effect::Emit(lines(&["ERROR"])));
            return 20;
        }
        self.call = Some(Call {
            direction: Direction::Outgoing,
            state: CallState::Dialling,
            number: number.to_string(),
            active_since: None,
            bearer: false,
        });
        self.at(520, Effect::Emit(lines(&["OK"])));
        if self.cfg.auto_network {
            self.at(560, Effect::Bearer);
            self.at(2200, Effect::Dialling);
            self.at(3300, Effect::Alerting);
        }
        520
    }

    fn answer(&mut self) -> Millis {
        if self.call.as_ref().map(|c| c.state) != Some(CallState::Incoming) {
            self.at(20, Effect::Emit(lines(&["NO CARRIER"])));
            return 20;
        }
        if self.cfg.auto_network {
            self.at(528, Effect::Bearer);
        }
        self.at(716, Effect::Connected);
        self.at(727, Effect::Emit(lines(&["OK"])));
        727
    }

    fn hang_up(&mut self) -> Millis {
        let done = match self.call.as_ref().map(|c| c.state) {
            None => 15,
            Some(CallState::Incoming | CallState::Waiting) => {
                self.missed(200);
                220
            }
            Some(_) => self.end_call(420) + 25,
        };
        self.at(done, Effect::Emit(lines(&["OK"])));
        done
    }
}

fn number_type(n: &str) -> u8 {
    if n.starts_with('+') {
        145
    } else {
        129
    }
}

fn clcc_line(c: &Call) -> String {
    let dir = match c.direction {
        Direction::Outgoing => 0,
        Direction::Incoming => 1,
    };
    let stat = match c.state {
        CallState::Active => 0,
        CallState::Held => 1,
        CallState::Dialling => 2,
        CallState::Alerting => 3,
        CallState::Incoming => 4,
        CallState::Waiting => 5,
        CallState::Disconnected => 6,
    };
    format!("+CLCC: 1,{dir},{stat},0,0,\"{}\",{},\"\"", c.number, number_type(&c.number))
}

/// The simulator as an L0 transport on a simulated clock. `read` advances
/// the clock (up to its timeout) until the modem has sent something, so
/// waits cost nothing in real time.
pub struct SimTransport {
    pub sim: Sim,
    now: Millis,
}

impl SimTransport {
    pub fn new(sim: Sim) -> Self {
        let now = sim.now();
        Self { sim, now }
    }

    /// Advance the clock without reading (e.g. to schedule a stimulus).
    pub fn sleep(&mut self, ms: Millis) {
        self.now += ms;
        self.sim.advance(self.now);
    }
}

impl Clock for SimTransport {
    fn now(&self) -> Millis {
        self.now
    }
}

impl Transport for SimTransport {
    fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.sim.write(bytes, self.now);
        Ok(())
    }

    fn read(&mut self, timeout: std::time::Duration) -> Option<Rx> {
        let target = self.now + timeout.as_millis() as Millis;
        loop {
            if let Some(b) = self.sim.take_rx() {
                return Some(Rx::Data(b));
            }
            match self.sim.next_event_at() {
                Some(t) if t <= target => {
                    self.now = self.now.max(t);
                    self.sim.advance(self.now);
                }
                _ => {
                    self.now = target;
                    self.sim.advance(self.now);
                    return self.sim.take_rx().map(Rx::Data);
                }
            }
        }
    }
}

#[cfg(test)]
mod replay;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::at::{AtEvent, AtParser};

    /// Drive the sim directly: returns the lines it sent up to `until`.
    struct Rig {
        sim: Sim,
        parser: AtParser,
    }

    impl Rig {
        fn on() -> Self {
            let mut sim = Sim::new(SimConfig::default());
            sim.power_on(0);
            sim.advance(20_000);
            let mut r = Rig { sim, parser: AtParser::new() };
            r.lines();
            r
        }

        fn send(&mut self, cmd: &str) {
            let now = self.sim.now();
            self.sim.write(format!("{cmd}\r").as_bytes(), now);
        }

        fn wait(&mut self, ms: Millis) -> Vec<String> {
            let t = self.sim.now() + ms;
            self.sim.advance(t);
            self.lines()
        }

        fn lines(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Some(b) = self.sim.take_rx() {
                for e in self.parser.feed(&b) {
                    if let AtEvent::Line(l) = e {
                        out.push(l);
                    }
                }
            }
            out
        }
    }

    #[test]
    fn boots_with_the_captured_sequence() {
        let mut sim = Sim::new(SimConfig::default());
        sim.power_on(1000);
        sim.write(b"AT\r", 2000); // before *ATREADY: ignored
        sim.advance(30_000);
        let mut p = AtParser::new();
        let got: Vec<_> = std::iter::from_fn(|| sim.take_rx())
            .flat_map(|b| p.feed(&b))
            .map(|e| match e {
                AtEvent::Line(l) => l,
                AtEvent::Prompt => "> ".into(),
            })
            .collect();
        assert_eq!(got, ["*ATREADY: 1", "+CPIN: READY", "SMS DONE", "+CGEV: EPS PDN ACT 1", "PB DONE"]);
    }

    #[test]
    fn echoes_until_ate0() {
        let mut r = Rig::on();
        r.send("AT");
        assert_eq!(r.wait(100), ["AT", "OK"]);
        r.send("ATE0");
        assert_eq!(r.wait(100), ["ATE0", "OK"]);
        r.send("AT");
        assert_eq!(r.wait(100), ["OK"]);
    }

    #[test]
    fn no_save_settings_reset_on_restart_auto_save_persist() {
        let mut r = Rig::on();
        for c in ["ATE0", "AT+CVHU=0", "AT+CLIP=1", "AT+COUTGAIN=7"] {
            r.send(c);
        }
        r.wait(1000);
        let t = r.sim.now();
        r.sim.cut_power(t);
        r.sim.power_on(t + 3000);
        r.wait(25_000);
        r.send("AT+CVHU?");
        r.send("AT+CLIP?");
        r.send("AT+COUTGAIN?");
        let got = r.wait(1000);
        assert_eq!(
            got,
            [
                "AT+CVHU?", "AT+CLIP?", "AT+COUTGAIN?", // echo back on
                "+CVHU: 1", "OK", "+CLIP: 1,1", "OK", "+COUTGAIN: 7", "OK"
            ]
        );
    }

    #[test]
    fn commands_queue_while_busy() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.wait(100);
        r.send("AT+COUTGAIN=7");
        r.send("AT+COUTGAIN?");
        assert_eq!(r.wait(100), [] as [&str; 0]); // 270 ms for the set
        assert_eq!(r.wait(500), ["OK", "+COUTGAIN: 7", "OK"]);
    }

    #[test]
    fn outgoing_call_answered_then_local_hang_up() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.wait(100);
        r.send("ATD07700900123;");
        assert_eq!(
            r.wait(4000),
            [
                "OK",
                "+CGEV: NW ACT 8,10",
                "+CLCC: 1,0,2,0,0,\"07700900123\",129,\"\"",
                "+CLCC: 1,0,3,0,0,\"07700900123\",129,\"\""
            ]
        );
        let t = r.sim.now();
        r.sim.remote_answer(t);
        assert_eq!(
            r.wait(100),
            [
                "VOICE CALL: BEGIN",
                "+CLCC: 1,0,0,0,0,\"07700900123\",129,\"\"",
                "+COLP: \"07700900123\",129"
            ]
        );
        r.wait(10_000);
        r.send("AT+CHUP");
        assert_eq!(
            r.wait(1000),
            [
                "+CGEV: NW DEACT 8,10",
                "+CLCC: 1,0,6,0,0,\"07700900123\",129,\"\"",
                "VOICE CALL: END: 000010",
                "NO CARRIER",
                "OK"
            ]
        );
        r.send("AT+CLCC");
        assert_eq!(r.wait(100), ["OK"]);
    }

    #[test]
    fn remote_hang_up() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.send("ATD07700900123;");
        r.wait(4000);
        let t = r.sim.now();
        r.sim.remote_answer(t);
        r.wait(35_000);
        let t = r.sim.now();
        r.sim.remote_hang_up(t);
        assert_eq!(
            r.wait(1000),
            [
                "+CGEV: NW DEACT 8,10",
                "+CLCC: 1,0,6,0,0,\"07700900123\",129,\"\"",
                "VOICE CALL: END: 000035",
                "NO CARRIER"
            ]
        );
    }

    #[test]
    fn incoming_call_rings_and_is_answered() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.send("AT+CLIP=1");
        r.wait(500);
        let t = r.sim.now();
        r.sim.incoming_call("07700900123", t);
        assert_eq!(
            r.wait(100),
            ["RING", "+CLIP: \"07700900123\",129", "+CLCC: 1,1,4,0,0,\"07700900123\",129,\"\""]
        );
        assert_eq!(r.wait(6000), ["RING", "+CLIP: \"07700900123\",129"]);
        r.send("ATA");
        assert_eq!(
            r.wait(1000),
            ["+CGEV: NW ACT 8,10", "VOICE CALL: BEGIN", "+CLCC: 1,1,0,0,0,\"07700900123\",129,\"\"", "OK"]
        );
        assert_eq!(r.wait(20_000), [] as [&str; 0], "no more rings once answered");
    }

    #[test]
    fn missed_call() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.wait(100);
        let t = r.sim.now();
        r.sim.incoming_call("07700900123", t);
        r.wait(13_000);
        let t = r.sim.now();
        r.sim.remote_hang_up(t);
        assert_eq!(
            r.wait(100),
            ["+CLCC: 1,1,6,0,0,\"07700900123\",129,\"\"", "VOICE CALL: END", "NO CARRIER"]
        );
        assert_eq!(r.wait(10_000), [] as [&str; 0]);
    }

    #[test]
    fn ath_is_ignored_for_voice_unless_cvhu_0() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.send("ATD07700900123;");
        r.wait(4000);
        let t = r.sim.now();
        r.sim.remote_answer(t);
        r.wait(100);
        r.send("ATH");
        assert_eq!(r.wait(500), ["OK"]);
        assert_eq!(r.sim.call_state(), Some(CallState::Active));
        r.send("AT+CVHU=0");
        r.send("ATH");
        let got = r.wait(1500);
        assert_eq!(got.last().map(String::as_str), Some("OK"));
        assert!(got.contains(&"NO CARRIER".to_string()));
        assert_eq!(r.sim.call_state(), None);
    }

    #[test]
    fn soft_power_off_and_hard_cut() {
        let mut r = Rig::on();
        let t = r.sim.now();
        r.sim.power_off(t);
        assert_eq!(r.wait(1000), ["+CGEV: ME DETACH"]);
        r.send("AT");
        assert_eq!(r.wait(1000), [] as [&str; 0]);

        let mut r = Rig::on();
        r.send("ATI");
        let t = r.sim.now();
        r.sim.cut_power(t); // reply was not yet out
        assert_eq!(r.wait(5000), [] as [&str; 0]);
    }

    #[test]
    fn errors() {
        let mut r = Rig::on();
        r.send("ATE0");
        r.wait(100);
        for c in ["AT+CLCC=1", "AT+NOPE", "AT+COUTGAIN=9"] {
            r.send(c);
            assert_eq!(r.wait(100), ["ERROR"], "{c}");
        }
        r.send("ATA");
        assert_eq!(r.wait(100), ["NO CARRIER"]);
    }

    #[test]
    fn transport_read_advances_the_simulated_clock() {
        let mut sim = Sim::new(SimConfig::default());
        sim.power_on(0);
        let mut t = SimTransport::new(sim);
        assert_eq!(t.read(std::time::Duration::from_secs(1)), None);
        assert_eq!(t.now(), 1000);
        assert_eq!(t.read(std::time::Duration::from_secs(10)), Some(Rx::Data(b"\r\n*ATREADY: 1\r\n".to_vec())));
        assert_eq!(t.now(), 6030);
    }
}
