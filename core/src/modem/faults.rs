//! Aggressive fault testing of L0 → L2 against the simulator. Test builds
//! only.
//!
//! [`FaultyLink`] sits where the UART would be: the simulator's output goes
//! through the real L0 [`RxQueue`] to the HAL, and on the way it can be
//! split, delayed, dropped (with the loss marker L0 would report), padded
//! with junk / empty lines / stray prompts, overflowed, or cut off by a
//! modem restart. It records ground truth: every URC that went onto the
//! wire, and every fault injected.
//!
//! [`Session`] runs the real HAL (via the shared driver loop) against it
//! with a randomised workload whose answers are checkable, and an
//! [`Oracle`] asserts after every step:
//!   - exactly one response per request;
//!   - every `Ok` response is true of the simulated modem: gain set/read
//!     values (tracked as the set of values the gain can have, given which
//!     sets definitely or possibly ran), `ATI` / firmware text, call list
//!     numbers. A reply credited to the wrong command fails this;
//!   - `Unknown` only if a fault happened since the request was submitted;
//!     `Malformed` only when undetectable corruption was allowed; `NotSent`
//!     never (the modem is never powered off here);
//!   - URCs: with no loss, exactly what went onto the wire, in order
//!     (`+CLCC` lines may legitimately be absorbed by an `AT+CLCC` reply,
//!     so those only as a subsequence); with loss, a subsequence;
//!   - liveness: once faults stop, every request completes and a fresh
//!     query succeeds.
//!
//! Physical limit: the UART (8N1, no flow control) has no checksum, so a
//! bit flip inside a line (`+COUTGAIN: 7` → `3`) or a fake final code is
//! undetectable at every layer. [`Faults::corrupt`] enables such damage;
//! sessions with it only check the contract and liveness, not truth.

use std::collections::{BTreeSet, HashMap, VecDeque};

use super::driver;
use super::hal::{Event, Hal, Output, RequestError, RequestId};
use super::link::{Failure, Millis};
use super::request::{Gain, PhoneNumber, Request, Response};
use super::sim::{Origin, Sim, SimConfig};
use super::transport::{Clock, Rx, RxQueue, Transport, WriteError};
use super::unsolicited::{self, CallState, Unsolicited};

pub(crate) const NUMBER: &str = "07700900123";
const ATI: &[&str] = &[
    "Manufacturer: INCORPORATED",
    "Model: A7682E",
    "Revision: A7682M7_V1.11.1",
    "IMEI: 000000000000000",
    "+GCAP: +CGSM,+FCLASS,+DS",
];

/// Small, seeded, reproducible PRNG (xorshift64*).
#[derive(Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// True with probability `p` (0.0 ..= 1.0).
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 <= p && p > 0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Split {
    /// Chunks as the simulator sent them.
    Whole,
    /// One byte per read: every boundary split at once.
    Bytes,
    /// Random piece sizes.
    Random,
}

/// Per-chunk fault probabilities, plus session-level ones (per second).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Faults {
    pub split: Split,
    /// Drop a random byte range, with the L0 loss marker (detected loss).
    pub drop: f64,
    /// Insert an unknown line at a line boundary.
    pub junk: f64,
    /// Insert empty lines (`\r\n`, `\r`, `\n` runs) at a line boundary.
    pub blank: f64,
    /// Insert a stray `> ` prompt at a line boundary.
    pub prompt: f64,
    /// Insert `RING` at a line boundary (a URC inside a reply).
    pub urc: f64,
    /// Per second: stall the modem's output long enough to time out a command.
    pub stall: f64,
    /// Per second: hard power cut, back on 3 s later.
    pub restart: f64,
    /// Per second: the consumer stops reading for a while (queue overflow).
    pub pause: f64,
    /// Undetectable damage: silent drops and bit flips.
    pub corrupt: f64,
    /// Capacity of the L0 queue (small to force overflow with `pause`).
    pub queue: usize,
}

impl Faults {
    pub(crate) fn none() -> Self {
        Faults {
            split: Split::Whole,
            drop: 0.0,
            junk: 0.0,
            blank: 0.0,
            prompt: 0.0,
            urc: 0.0,
            stall: 0.0,
            restart: 0.0,
            pause: 0.0,
            corrupt: 0.0,
            queue: 16 * 1024,
        }
    }
}

/// The wire between the simulated modem and the HAL, with fault injection.
pub(crate) struct FaultyLink {
    pub sim: Sim,
    now: Millis,
    pub faults: Faults,
    pub faults_on: bool,
    rng: Rng,
    queue: RxQueue,
    /// Chunks the modem sent that are held back by a stall.
    wire: VecDeque<(Origin, Vec<u8>)>,
    carry: VecDeque<Rx>,
    stall_until: Millis,
    pause_until: Millis,
    power_on_at: Option<Millis>,
    /// Ground truth: every URC line put onto the wire (incl. injected), in order.
    pub urcs_sent: Vec<String>,
    /// Every line put onto the wire, any origin, with the time: a reply line
    /// that arrives after its command timed out legitimately surfaces as a
    /// URC.
    pub lines_sent: Vec<(Millis, Origin, String)>,
    /// When each fault's effects may make a request `Unknown`: from the
    /// fault until it can no longer matter (end of the stall, the boot after
    /// a power cut, the consumer reading the overflowed queue again).
    pub disruptions: Vec<(Millis, Millis)>,
    /// Loss markers reported, or bytes damaged undetectably.
    pub losses: u64,
    pub corrupted: bool,
    pub log: Vec<(Millis, String)>,
    /// Recent traffic, for failure messages.
    pub traffic: VecDeque<String>,
}

impl FaultyLink {
    pub(crate) fn new(cfg: SimConfig, faults: Faults, seed: u64) -> Self {
        FaultyLink {
            sim: Sim::new(cfg),
            now: 0,
            faults,
            faults_on: true,
            rng: Rng::new(seed),
            queue: RxQueue::new(faults.queue),
            wire: VecDeque::new(),
            carry: VecDeque::new(),
            stall_until: 0,
            pause_until: 0,
            power_on_at: None,
            urcs_sent: Vec::new(),
            lines_sent: Vec::new(),
            disruptions: Vec::new(),
            losses: 0,
            corrupted: false,
            log: Vec::new(),
            traffic: VecDeque::new(),
        }
    }

    fn disrupt(&mut self, until: Millis) {
        self.disruptions.push((self.now, until));
    }

    /// Whether a fault could have affected something in flight during `[from, to]`.
    pub(crate) fn disrupted(&self, from: Millis, to: Millis) -> bool {
        self.disruptions.iter().any(|&(a, b)| a <= to && b >= from)
    }

    fn traffic(&mut self, what: String) {
        if self.traffic.len() == 120 {
            self.traffic.pop_front();
        }
        self.traffic.push_back(format!("{} {what}", self.now));
    }

    fn note(&mut self, what: String) {
        self.log.push((self.now, what));
    }

    /// Session-level faults, rolled once per simulated second.
    pub(crate) fn roll_session_faults(&mut self, dt_ms: Millis) {
        if !self.faults_on {
            return;
        }
        let per = |p: f64| p * dt_ms as f64 / 1000.0;
        if self.rng.chance(per(self.faults.stall)) {
            let d = 9_500 + self.rng.below(3_000); // past the 9 s command timeout
            self.stall_until = self.now + d;
            self.disrupt(self.stall_until + 1000);
            self.note(format!("stall {d} ms"));
        }
        if self.power_on_at.is_none() && self.rng.chance(per(self.faults.restart)) {
            self.sim.cut_power(self.now);
            self.power_on_at = Some(self.now + 3000);
            self.disrupt(self.now + 3000 + 10_000); // boot ~6 s after power returns
            self.note("power cut".into());
        }
        if self.rng.chance(per(self.faults.pause)) {
            let d = 500 + self.rng.below(4_000);
            self.pause_until = self.now + d;
            // A long enough pause delays a reply past its timeout, even
            // without overflowing the queue.
            self.disrupt(self.pause_until + 1000);
            self.note(format!("consumer paused {d} ms"));
        }
    }

    /// Take what the modem sent, apply faults, feed the L0 queue.
    fn pump(&mut self) {
        while let Some(chunk) = self.sim.take_rx_tagged() {
            self.wire.push_back(chunk);
        }
        if self.now < self.stall_until {
            return;
        }
        while let Some((origin, bytes)) = self.wire.pop_front() {
            let pieces = self.damage(origin, bytes);
            for p in pieces {
                match p {
                    Piece::Bytes(b) => {
                        if self.queue.push(&b) < b.len() {
                            self.disrupt(self.pause_until.max(self.now) + 1000);
                            self.losses += 1;
                            self.note("queue overflow".into());
                        }
                    }
                    Piece::Lost => {
                        self.queue.mark_lost();
                        self.losses += 1;
                        self.disrupt(self.pause_until.max(self.now) + 1000);
                    }
                }
            }
        }
    }

    /// Split a chunk into lines-with-terminators, record ground truth, and
    /// apply per-chunk faults at line boundaries.
    fn damage(&mut self, origin: Origin, bytes: Vec<u8>) -> Vec<Piece> {
        let f = self.faults;
        let on = self.faults_on;
        // Record URC lines (each chunk is whole lines; echo has a bare \r).
        let mut out: Vec<Piece> = Vec::new();
        let boundary_insert = |me: &mut Self, out: &mut Vec<Piece>| {
            if !on {
                return;
            }
            if me.rng.chance(f.junk) {
                out.push(Piece::Bytes(format!("\r\nJUNK{}\r\n", me.rng.below(1000)).into_bytes()));
            }
            if me.rng.chance(f.blank) {
                let runs: [&[u8]; 4] = [b"\r\n", b"\r", b"\n", b"\r\n\r\n\n\r"];
                out.push(Piece::Bytes(runs[me.rng.below(4) as usize].to_vec()));
            }
            if me.rng.chance(f.prompt) {
                out.push(Piece::Bytes(b"\r\n> ".to_vec()));
            }
            if me.rng.chance(f.urc) {
                me.urcs_sent.push("RING".into());
                me.lines_sent.push((me.now, Origin::Urc, "RING".into()));
                out.push(Piece::Bytes(b"\r\nRING\r\n".to_vec()));
            }
        };
        for line in split_lines(&bytes) {
            boundary_insert(self, &mut out);
            let text = String::from_utf8_lossy(&line);
            let t = text.trim_matches(['\r', '\n']);
            if !t.is_empty() {
                self.lines_sent.push((self.now, origin, t.to_string()));
                if origin == Origin::Urc {
                    self.urcs_sent.push(t.to_string());
                }
            }
            let mut line = line;
            if on && self.rng.chance(f.corrupt) && !line.is_empty() {
                self.corrupted = true;
                self.losses += 1;
                self.disrupt(self.now + 10_000);
                let i = self.rng.below(line.len() as u64) as usize;
                if self.rng.chance(0.5) {
                    line.remove(i); // silent drop
                } else {
                    line[i] ^= 1 << self.rng.below(7); // bit flip
                }
                self.note("corrupted a line".into());
            }
            if on && self.rng.chance(f.drop) && !line.is_empty() {
                let a = self.rng.below(line.len() as u64) as usize;
                let b = a + 1 + self.rng.below((line.len() - a) as u64) as usize;
                out.push(Piece::Bytes(line[..a].to_vec()));
                out.push(Piece::Lost);
                out.push(Piece::Bytes(line[b..].to_vec()));
                self.note(format!("dropped {} bytes", b - a));
            } else {
                out.push(Piece::Bytes(line));
            }
        }
        boundary_insert(self, &mut out);
        out
    }

    fn deliver(&mut self) -> Option<Rx> {
        if self.now < self.pause_until {
            return None;
        }
        let rx = self.carry.pop_front().or_else(|| self.queue.pop())?;
        let Rx::Data(d) = rx else {
            self.traffic("< LOST".into());
            return Some(Rx::Lost);
        };
        let cut = match self.faults.split {
            Split::Whole => d.len(),
            Split::Bytes => 1,
            Split::Random => 1 + self.rng.below(d.len() as u64) as usize,
        };
        if cut < d.len() {
            self.carry.push_front(Rx::Data(d[cut..].to_vec()));
        }
        self.traffic(format!("< {:?}", String::from_utf8_lossy(&d[..cut])));
        Some(Rx::Data(d[..cut].to_vec()))
    }

    fn advance_to(&mut self, t: Millis) {
        self.now = self.now.max(t);
        if let Some(on) = self.power_on_at.filter(|&on| on <= self.now) {
            self.sim.advance(on);
            self.sim.power_on(on);
            self.power_on_at = None;
            self.note("power restored".into());
        }
        self.sim.advance(self.now);
        self.pump();
    }

    fn next_wake(&self) -> Option<Millis> {
        let mut t = self.sim.next_event_at();
        for c in [
            Some(self.stall_until).filter(|&s| s > self.now),
            Some(self.pause_until).filter(|&p| p > self.now),
            self.power_on_at,
        ]
        .into_iter()
        .flatten()
        {
            t = Some(t.map_or(c, |x| x.min(c)));
        }
        t
    }
}

enum Piece {
    Bytes(Vec<u8>),
    Lost,
}

/// Split after each `\n` (and after a bare `\r` not followed by `\n`), so
/// insertions land between lines, never inside one.
fn split_lines(b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for (i, &c) in b.iter().enumerate() {
        cur.push(c);
        let end = c == b'\n' || (c == b'\r' && b.get(i + 1) != Some(&b'\n'));
        if end {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

impl Clock for FaultyLink {
    fn now(&self) -> Millis {
        self.now
    }
}

impl Transport for FaultyLink {
    fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        self.traffic(format!("> {:?}", String::from_utf8_lossy(bytes)));
        self.sim.write(bytes, self.now);
        Ok(())
    }

    fn read(&mut self, timeout: std::time::Duration) -> Option<Rx> {
        let target = self.now + timeout.as_millis() as Millis;
        loop {
            self.pump();
            if let Some(rx) = self.deliver() {
                return Some(rx);
            }
            match self.next_wake().filter(|&t| t <= target) {
                Some(t) => self.advance_to(t.max(self.now)),
                None => {
                    self.advance_to(target);
                    return self.deliver();
                }
            }
        }
    }
}

/// Checks every HAL output against the simulated modem's truth.
pub(crate) struct Oracle {
    /// Values the gain may have, given which sets definitely / possibly ran.
    allowed_gain: BTreeSet<u8>,
    /// Request and when it was submitted.
    outstanding: HashMap<RequestId, (Request, Millis)>,
    pub delivered_urcs: Vec<(Millis, String)>,
    pub oks: u64,
    pub unknowns: u64,
    /// `ATA` that found no call left (see `check_done`): legitimate, so
    /// counted apart from `unknowns`.
    pub answer_timeouts: u64,
    pub configured: u64,
    pub restarts: u64,
}

impl Oracle {
    fn new() -> Self {
        Oracle {
            allowed_gain: [4, 7].into(),
            outstanding: HashMap::new(),
            delivered_urcs: Vec::new(),
            oks: 0,
            unknowns: 0,
            answer_timeouts: 0,
            configured: 0,
            restarts: 0,
        }
    }

    fn check(&mut self, o: Output, link: &FaultyLink) -> Result<(), String> {
        match o {
            Output::Write(_) => unreachable!(),
            Output::Event(e) => {
                match &e {
                    Event::Urc(Unsolicited::Unrecognised(_)) => {}
                    Event::Urc(u) => self.delivered_urcs.push((link.now(), urc_text(u))),
                    Event::Restarted => {
                        self.delivered_urcs.push((link.now(), "*ATREADY: 1".into()));
                        self.restarts += 1;
                        self.allowed_gain.insert(7);
                    }
                    Event::LinkLost(_) => {
                        self.allowed_gain.insert(7);
                    }
                    Event::Configured { issues } => {
                        self.configured += 1;
                        self.allowed_gain.insert(7);
                        // Only SIM-dependent settings may fail (caller ID before
                        // the SIM is ready, SMS before `SMS DONE`), unless
                        // undetectable corruption is in play.
                        let sim_dependent = ["AT+CLIP", "AT+CMGF", "AT+CNMI"];
                        if !link.corrupted && issues.iter().any(|i| !sim_dependent.iter().any(|p| i.starts_with(p))) {
                            return Err(format!("unexpected config issues {issues:?}"));
                        }
                    }
                    Event::BytesLost | Event::UnexpectedPrompt => {}
                }
                Ok(())
            }
            Output::Done { id, result } => {
                let (req, submitted) = self.outstanding.remove(&id).ok_or_else(|| format!("Done for unknown or finished request {id:?}"))?;
                self.check_done(&req, submitted, result, link)
            }
        }
    }

    fn check_done(
        &mut self,
        req: &Request,
        submitted: Millis,
        result: Result<Response, RequestError>,
        link: &FaultyLink,
    ) -> Result<(), String> {
        let disrupted = link.disrupted(submitted, link.now());
        match (req, result) {
            // ATA with no call left to answer (the caller gave up) may get
            // only `NO CARRIER`, which is a URC: the answer times out.
            (Request::Answer, Err(RequestError::Unknown(super::link::UnknownReason::Timeout))) => {
                self.answer_timeouts += 1;
                Ok(())
            }
            (_, Err(RequestError::Unknown(why))) => {
                if !disrupted {
                    return Err(format!("{req:?}: Unknown({why:?}) with no fault while it was pending (since {submitted} ms)"));
                }
                self.unknowns += 1;
                if let Request::SetOutputGain(g) = req {
                    self.allowed_gain.insert(g.level());
                }
                Ok(())
            }
            (_, Err(RequestError::NotSent)) => Err(format!("{req:?}: NotSent, but the modem is never off")),
            (_, Err(RequestError::Malformed(l))) if link.corrupted => {
                let _ = l;
                Ok(())
            }
            (_, Err(RequestError::Malformed(l))) => Err(format!("{req:?}: Malformed {l:?} without corruption")),
            (Request::Dial(_) | Request::Answer, Err(RequestError::Failed(_))) => Ok(()),
            (_, Err(RequestError::Failed(f))) if link.corrupted => {
                let _: Failure = f;
                Ok(())
            }
            (_, Err(e)) => Err(format!("{req:?}: unexpected {e:?}")),
            (req, Ok(resp)) => {
                self.oks += 1;
                if link.corrupted {
                    return Ok(()); // truth unknowable under undetectable damage
                }
                self.check_truth(req, resp)
            }
        }
    }

    fn check_truth(&mut self, req: &Request, resp: Response) -> Result<(), String> {
        match (req, &resp) {
            (Request::SetOutputGain(g), Response::Done) => {
                self.allowed_gain = [g.level()].into();
                Ok(())
            }
            (Request::QueryOutputGain, Response::Gain(x)) => {
                if !self.allowed_gain.contains(&x.level()) {
                    return Err(format!("gain read {} but it can only be {:?}", x.level(), self.allowed_gain));
                }
                self.allowed_gain = [x.level()].into();
                Ok(())
            }
            (Request::Firmware, Response::Firmware(v)) if v == "A011B15A7682M7" => Ok(()),
            (Request::Identify, Response::Lines(l)) if l.iter().map(String::as_str).eq(ATI.iter().copied()) => Ok(()),
            (Request::ListCalls, Response::Calls(calls))
                if calls.len() <= 1 && calls.iter().all(|c| c.number.as_deref() == Some(NUMBER)) =>
            {
                Ok(())
            }
            (Request::Dial(_) | Request::Answer | Request::HangUp, Response::Done) => Ok(()),
            _ => Err(format!("{req:?} answered with {resp:?}")),
        }
    }
}

/// A recognised URC back to its wire text, for comparison with ground truth.
fn urc_text(u: &Unsolicited) -> String {
    use Unsolicited as U;
    match u {
        U::Ready => "*ATREADY: 1".into(),
        U::Sim(s) => format!("+CPIN: {s}"),
        U::SmsReady => "SMS DONE".into(),
        U::PhonebookReady => "PB DONE".into(),
        U::Ring => "RING".into(),
        U::CallerId { number } => format!("+CLIP: \"{}\",129", number.clone().unwrap_or_default()),
        U::ConnectedLine { number } => format!("+COLP: \"{}\",129", number.clone().unwrap_or_default()),
        U::Call(c) => format!("CLCC {} {:?} {:?}", c.index, c.direction, c.state),
        U::VoiceCallBegin => "VOICE CALL: BEGIN".into(),
        U::VoiceCallEnd { duration_s: Some(_) } => "VOICE CALL: END: <d>".into(),
        U::VoiceCallEnd { duration_s: None } => "VOICE CALL: END".into(),
        U::NoCarrier => "NO CARRIER".into(),
        U::Busy => "BUSY".into(),
        U::NoAnswer => "NO ANSWER".into(),
        U::NewSms { storage, index } => format!("+CMTI: \"{storage}\",{index}"),
        U::PacketDomain(s) => format!("+CGEV: {s}"),
        U::Unrecognised(s) => s.clone(),
    }
}

/// Ground-truth URC line in the same normal form as `urc_text`.
fn normalise_sent(line: &str) -> Option<String> {
    match unsolicited::parse(line) {
        Unsolicited::Unrecognised(_) => None,
        u => Some(urc_text(&u)),
    }
}

/// A HAL + workload + oracle run against a [`FaultyLink`].
pub(crate) struct Session {
    pub hal: Hal,
    pub link: FaultyLink,
    pub oracle: Oracle,
    rng: Rng,
    out: Vec<Output>,
    last_gain_set: u8,
    call_since: Option<(CallState, Millis)>,
}

impl Session {
    /// Modem and HAL powered on together.
    pub(crate) fn new(cfg: SimConfig, faults: Faults, seed: u64) -> Self {
        let mut s = Session {
            hal: Hal::new(),
            link: FaultyLink::new(cfg, faults, seed),
            oracle: Oracle::new(),
            rng: Rng::new(seed ^ 0xABCD),
            out: Vec::new(),
            last_gain_set: 7,
            call_since: None,
        };
        s.link.sim.power_on(0);
        s.hal.power_on(0);
        s
    }

    pub(crate) fn submit(&mut self, r: Request) -> RequestId {
        let id = self.hal.submit(r.clone(), self.link.now());
        self.oracle.outstanding.insert(id, (r, self.link.now()));
        id
    }

    /// One driver step plus oracle checks.
    pub(crate) fn step(&mut self) -> Result<(), String> {
        let before = self.link.now();
        driver::step(&mut self.hal, &mut self.link, 20, &mut self.out).map_err(|e| format!("{e:?}"))?;
        let dt = self.link.now() - before;
        self.link.roll_session_faults(dt.max(1));
        for o in std::mem::take(&mut self.out) {
            self.oracle.check(o, &self.link)?;
        }
        Ok(())
    }

    /// Random requests and far-end actions, then a step.
    pub(crate) fn step_with_workload(&mut self, calls: bool) -> Result<(), String> {
        if self.hal.is_ready() && self.oracle.outstanding.len() < 3 && self.rng.chance(0.02) {
            let r = match self.rng.below(10) {
                0..=2 => {
                    let mut g = self.rng.below(7) as u8; // 0..=6; 7 is the config value
                    if g == self.last_gain_set {
                        g = (g + 1) % 7;
                    }
                    self.last_gain_set = g;
                    Request::SetOutputGain(Gain::new(g).unwrap())
                }
                3..=5 => Request::QueryOutputGain,
                6 => Request::Firmware,
                7 => Request::Identify,
                _ => Request::ListCalls,
            };
            self.submit(r);
        }
        if calls {
            self.call_workload();
        }
        self.step()
    }

    fn call_workload(&mut self) {
        let now = self.link.now();
        let state = self.link.sim.call_state();
        let since = match (state, self.call_since) {
            (Some(s), Some((p, t))) if s == p => now - t,
            (Some(s), _) => {
                self.call_since = Some((s, now));
                0
            }
            (None, _) => {
                self.call_since = None;
                0
            }
        };
        if !self.hal.is_ready() || !self.rng.chance(0.01) {
            return;
        }
        let busy = self.oracle.outstanding.values().any(|(r, _)| matches!(r, Request::Dial(_) | Request::Answer | Request::HangUp));
        match state {
            None if !busy => {
                if self.rng.chance(0.5) {
                    self.submit(Request::Dial(PhoneNumber::parse(NUMBER).unwrap()));
                } else {
                    self.link.sim.incoming_call(NUMBER, now);
                }
            }
            Some(CallState::Alerting) if since > 1000 => self.link.sim.remote_answer(now),
            Some(CallState::Incoming) if since > 1000 && !busy => {
                if self.rng.chance(0.7) {
                    self.submit(Request::Answer);
                } else {
                    self.link.sim.remote_hang_up(now);
                }
            }
            Some(CallState::Active) if since > 3000 && !busy => {
                if self.rng.chance(0.5) {
                    self.submit(Request::HangUp);
                } else {
                    self.link.sim.remote_hang_up(now);
                }
            }
            _ => {}
        }
    }

    /// Run for `ms` of simulated time with the workload.
    pub(crate) fn run(&mut self, ms: Millis, calls: bool) -> Result<(), String> {
        let end = self.link.now() + ms;
        while self.link.now() < end {
            self.step_with_workload(calls)?;
        }
        Ok(())
    }

    /// Stop injecting faults, let everything settle, and check liveness:
    /// all requests answered, a fresh query succeeds.
    pub(crate) fn settle(&mut self) -> Result<(), String> {
        self.link.faults_on = false;
        let end = self.link.now() + 60_000;
        while self.link.now() < end && !(self.oracle.outstanding.is_empty() && self.hal.is_ready()) {
            self.step()?;
        }
        if !self.oracle.outstanding.is_empty() {
            return Err(format!("requests never answered: {:?}", self.oracle.outstanding));
        }
        let id = self.submit(Request::QueryOutputGain);
        let end = self.link.now() + 30_000;
        while self.link.now() < end && self.oracle.outstanding.contains_key(&id) {
            self.step()?;
        }
        if self.oracle.outstanding.contains_key(&id) {
            return Err("fresh query after settling was not answered".into());
        }
        Ok(())
    }

    /// URC delivery: exact (modulo absorbed `+CLCC`) with no loss, a
    /// subsequence otherwise.
    pub(crate) fn check_urcs(&mut self) -> Result<(), String> {
        let got: Vec<(Millis, String)> = self.oracle.delivered_urcs.clone();
        let got_text: Vec<String> = got.iter().map(|(_, l)| l.clone()).collect();
        // Subsequence of every line on the wire that reads as a URC (late
        // reply lines included).
        let wire: Vec<(Millis, String)> =
            self.link.lines_sent.iter().filter_map(|(t, _, l)| normalise_sent(l).map(|n| (*t, n))).collect();
        let wire_text: Vec<String> = wire.iter().map(|(_, l)| l.clone()).collect();
        if let Err((i, j)) = subsequence(&got_text, &wire_text) {
            let w = |v: &[(Millis, String)], k: usize| v[k.saturating_sub(4)..(k + 4).min(v.len())].to_vec();
            return Err(format!(
                "delivered URC #{i} {:?} was never sent (after wire #{j})\n got around it:  {:?}\n wire from there: {:?}",
                got[i],
                w(&got, i),
                w(&wire, j)
            ));
        }
        // With no loss: exactly the URCs sent (modulo `+CLCC`, which a reply
        // may absorb or a late reply may add), plus reply lines no command
        // accepts: `NO CARRIER` answering an `ATA` with no call left.
        if self.link.losses == 0 {
            let sent: Vec<String> = self
                .link
                .lines_sent
                .iter()
                .filter(|(_, o, l)| *o == Origin::Urc || l == "NO CARRIER")
                .filter_map(|(_, _, l)| normalise_sent(l))
                .collect();
            let strip = |v: &[String]| v.iter().filter(|l| !l.starts_with("CLCC ")).cloned().collect::<Vec<_>>();
            let (g, s) = (strip(&got_text), strip(&sent));
            if g != s {
                let i = g.iter().zip(&s).position(|(a, b)| a != b).unwrap_or(g.len().min(s.len()));
                return Err(format!(
                    "URCs differ without any loss reported, at #{i}\n got:  {:?}\n sent: {:?}",
                    &g[i.saturating_sub(3)..(i + 4).min(g.len())],
                    &s[i.saturating_sub(3)..(i + 4).min(s.len())]
                ));
            }
        }
        Ok(())
    }

    /// Full run: workload with faults, settle, URC check. Errors carry the
    /// fault log tail and recent traffic for diagnosis.
    pub(crate) fn soak(&mut self, ms: Millis, calls: bool) -> Result<(), String> {
        let r = self.run(ms, calls).and_then(|_| self.settle()).and_then(|_| self.check_urcs());
        r.map_err(|e| self.diagnose(e))
    }

    pub(crate) fn diagnose(&self, e: String) -> String {
        let now = self.link.now();
        let tail: Vec<_> = self.link.log.iter().filter(|(t, _)| *t + 30_000 >= now).collect();
        let traffic: Vec<_> = self.link.traffic.iter().cloned().collect();
        format!("{e}\n at {} ms; recent faults: {tail:?}\n traffic:\n  {}", self.link.now(), traffic.join("\n  "))
    }
}

/// `Ok` if `sub` is a subsequence of `of`; else the index in `sub` that
/// could not be matched and how far into `of` matching had got.
fn subsequence(sub: &[String], of: &[String]) -> Result<(), (usize, usize)> {
    let mut j = 0;
    for (i, x) in sub.iter().enumerate() {
        match of[j..].iter().position(|y| y == x) {
            Some(k) => j += k + 1,
            None => return Err((i, j)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
