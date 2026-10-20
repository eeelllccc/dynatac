//! The simulator must behave like the real modem in every capture.
//!
//! For each fixture the simulator gets the same inputs the real modem got:
//!   - what we wrote, when we wrote it;
//!   - power events (from the capture tool's notes);
//!   - what the network and the far end did, read off the trace: dial
//!     progress (`+CLCC` stat 2/3 URCs), the voice bearer (`+CGEV: NW ACT`),
//!     the far end answering (`VOICE CALL: BEGIN` with no `ATA`), hanging
//!     up (`+CGEV: NW DEACT` with no `AT+CHUP`), calling (first `RING`) or
//!     giving up (`+CLCC` stat 6 on an unanswered incoming call), an SMS
//!     arriving (`+CMTI`, with the PDU the trace later lists at that index);
//!   - the SIM's contents at the start: messages the first listing shows
//!     that weren't announced before it, and the message reference counter
//!     (the first `+CMGS: <mr>`).
//!
//! Everything else is the simulator's own behaviour, and its output lines
//! must equal the real modem's, in order, each within `TOLERANCE_MS` of
//! the real time (call durations within 1 s). Two captured races are allowed: whether the bearer URC
//! lands just before or just after `ATD`'s `OK`, and on hang-up whether the
//! bearer goes down just before or just after `+CLCC` stat 6 (see
//! `golden_traces.rs`).

use crate::modem::at::{AtEvent, AtParser};
use crate::modem::link::Millis;
use crate::modem::sim::{Sim, SimConfig, SimSms};
use crate::modem::sms_storage::{self, Status};
use crate::modem::trace::{parse_trace, timeline, Direction, Step, TraceRecord};

macro_rules! fixture {
    ($name:literal) => {
        ($name, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/modem/", $name, ".trace")))
    };
}

const ALL: &[(&str, &str)] = &[
    fixture!("01-boot"),
    fixture!("02-out-answered-remote-hangup"),
    fixture!("03-out-answered-voicemail-hangup"),
    fixture!("03-out-answered-local-hangup"),
    fixture!("04-out-rejected"),
    fixture!("05-out-unanswered"),
    fixture!("06-out-busy-or-unreachable"),
    fixture!("07-in-answered"),
    fixture!("08-in-missed"),
    fixture!("09-sms-during-call"),
    fixture!("10-soft-power-cycle"),
    fixture!("11-hard-power-cut"),
    fixture!("12-command-during-urc"),
    fixture!("14-sms-receive-idle"),
    fixture!("15-sms-receive-in-call"),
    fixture!("16-sms-send"),
];

/// How far each simulated line may be from the real one. The boot-time
/// PDN activation is network-timed (10.2–11.5 s after power-on across
/// the captures), so it gets more room.
const TOLERANCE_MS: u64 = 300;
const NETWORK_TIMED_TOLERANCE_MS: u64 = 1200;
const NETWORK_TIMED: &str = "+CGEV: EPS PDN ACT 1";
const NUMBER: &str = "07700900123";

#[derive(Debug)]
enum Input {
    Write(Vec<u8>),
    PowerOn,
    PowerOff,
    CutPower,
    Dialling,
    Alerting,
    Bearer,
    RemoteAnswer,
    RemoteHangUp,
    IncomingCall,
    ReceiveSms(String),
    /// On the SIM before the trace starts.
    Stored(SimSms),
    NextMessageReference(u8),
}

/// Every stored message the trace lists (`+CMGL` / `+CMGR` + PDU line):
/// (time, index if listed, status, PDU), in trace order.
fn listed(records: &[TraceRecord]) -> Vec<(Millis, Option<u32>, Status, String)> {
    let lines: Vec<(Millis, String)> = timeline(records)
        .into_iter()
        .filter_map(|s| match s.step {
            Step::Line(l) => Some((s.t_ms, l)),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for pair in lines.windows(2) {
        let ((t, head), (_, pdu)) = (&pair[0], &pair[1]);
        let info = [head.clone(), pdu.clone()];
        if let Some(m) = sms_storage::parse_list(&info).and_then(|mut l| l.pop()) {
            out.push((*t, Some(m.index), m.status, m.pdu));
        } else if let Some(Some(m)) = sms_storage::parse_read(0, &info) {
            out.push((*t, None, m.status, m.pdu));
        }
    }
    out
}

/// SMS inputs: arrivals at each `+CMTI` (PDU from the later listing),
/// the SIM's starting contents, and the message reference counter.
fn sms_inputs(records: &[TraceRecord]) -> Vec<(Millis, Input)> {
    let listed = listed(records);
    let pdu_at = |index: u32| listed.iter().find(|(_, i, _, _)| *i == Some(index)).map(|(_, _, _, p)| p.clone());
    let mut out = Vec::new();
    let mut announced = Vec::new();
    for s in timeline(records) {
        if let Step::Line(l) = &s.step {
            if let Some(i) = l.strip_prefix("+CMTI: \"SM\",").and_then(|i| i.parse::<u32>().ok()) {
                out.push((s.t_ms, Input::ReceiveSms(pdu_at(i).expect("an announced message is listed later"))));
                announced.push((s.t_ms, i));
            }
        }
    }
    // The first listing: its indices count up; the next listing starts over.
    let mut first_listing: Vec<_> = listed.iter().filter(|(_, i, _, _)| i.is_some()).collect();
    if let Some(end) = first_listing.windows(2).position(|w| w[1].1 <= w[0].1) {
        first_listing.truncate(end + 1);
    }
    for (t, index, status, pdu) in first_listing {
        let index = index.unwrap();
        if !announced.iter().any(|&(at, i)| i == index && at < *t) {
            out.push((0, Input::Stored(SimSms { index, status: *status, pdu: pdu.clone() })));
        }
    }
    let mr = timeline(records).into_iter().find_map(|s| match s.step {
        Step::Line(l) => l.strip_prefix("+CMGS: ").and_then(|m| m.parse().ok()),
        _ => None,
    });
    out.extend(mr.map(|m| (0, Input::NextMessageReference(m))));
    out
}

/// The inputs the real modem received, read off the trace.
fn inputs(records: &[TraceRecord]) -> Vec<(Millis, Input)> {
    let mut out = Vec::new();
    for r in records {
        match (r.dir, r.bytes.as_slice()) {
            (Direction::Tx, b) => out.push((r.t_ms, Input::Write(b.to_vec()))),
            (Direction::Note, b"pwrkey on-pulse end") => out.push((r.t_ms, Input::PowerOn)),
            (Direction::Note, b"pwrkey off-pulse end") => out.push((r.t_ms, Input::PowerOff)),
            (Direction::Note, b"power_en low") => out.push((r.t_ms, Input::CutPower)),
            _ => {}
        }
    }
    let mut in_clcc_reply = false;
    let mut chup_at: Option<Millis> = None;
    let mut answered_locally = false;
    let mut ringing = false;
    for s in timeline(records) {
        let t = s.t_ms;
        match &s.step {
            Step::Sent(c) => {
                in_clcc_reply = c == "AT+CLCC";
                if c == "AT+CHUP" {
                    chup_at = Some(t);
                }
                if c == "ATA" {
                    answered_locally = true;
                }
            }
            Step::Line(l) => {
                let l = l.as_str();
                if in_clcc_reply {
                    if l == "OK" || l == "ERROR" {
                        in_clcc_reply = false;
                    }
                    continue;
                }
                let input = match l {
                    "+CGEV: NW ACT 8,10" => Some(Input::Bearer),
                    "VOICE CALL: BEGIN" if !answered_locally => Some(Input::RemoteAnswer),
                    "+CGEV: NW DEACT 8,10" if !chup_at.is_some_and(|c| t - c < 1000) => {
                        Some(Input::RemoteHangUp)
                    }
                    "RING" if !ringing => {
                        ringing = true;
                        Some(Input::IncomingCall)
                    }
                    "NO CARRIER" => {
                        ringing = false;
                        answered_locally = false;
                        None
                    }
                    _ if l.starts_with("+CLCC: 1,0,2,") => Some(Input::Dialling),
                    _ if l.starts_with("+CLCC: 1,0,3,") => Some(Input::Alerting),
                    _ if l.starts_with("+CLCC: 1,1,6,") && !answered_locally && !chup_at.is_some_and(|c| t - c < 1000) => {
                        Some(Input::RemoteHangUp)
                    }
                    _ => None,
                };
                if let Some(i) = input {
                    out.push((t, i));
                }
            }
            _ => {}
        }
    }
    out.extend(sms_inputs(records));
    out.sort_by_key(|(t, _)| *t); // stable: same-time inputs keep trace order
    out
}

fn run_sim(inputs: Vec<(Millis, Input)>, until: Millis) -> Vec<(Millis, String)> {
    let mut sim = Sim::new(SimConfig { auto_network: false, ..SimConfig::default() });
    for (t, input) in inputs {
        match input {
            Input::Write(b) => sim.write(&b, t),
            Input::PowerOn => sim.power_on(t),
            Input::PowerOff => sim.power_off(t),
            Input::CutPower => sim.cut_power(t),
            Input::Dialling => sim.network_dialling(t),
            Input::Alerting => sim.network_alerting(t),
            Input::Bearer => sim.network_bearer(t),
            Input::RemoteAnswer => sim.remote_answer(t),
            Input::RemoteHangUp => sim.remote_hang_up(t),
            Input::IncomingCall => sim.incoming_call(NUMBER, t),
            Input::ReceiveSms(pdu) => {
                sim.receive_sms(&pdu, t).expect("the simulated SIM stores it");
            }
            Input::Stored(m) => sim.store_sms(m),
            Input::NextMessageReference(mr) => sim.set_next_message_reference(mr),
        }
    }
    sim.advance(until);
    let mut parser = AtParser::new();
    let mut lines = Vec::new();
    while let Some((t, b)) = sim.take_rx_timed() {
        for e in parser.feed(&b) {
            if let AtEvent::Line(l) = e {
                lines.push((t, l));
            }
        }
    }
    lines
}

/// The captured races: `+CGEV: NW ACT` just before `ATD`'s `OK` is the
/// same behaviour as just after it; on hang-up, `+CGEV: NW DEACT` before
/// the `+CLCC` stat 6 the same as just after it.
fn normalise(mut lines: Vec<(Millis, String)>) -> Vec<(Millis, String)> {
    let call_ended = |l: &str| l.starts_with("+CLCC: ") && l.split(',').nth(2) == Some("6");
    for i in 0..lines.len().saturating_sub(1) {
        let (a, b) = (lines[i].1.as_str(), lines[i + 1].1.as_str());
        if (a == "+CGEV: NW ACT 8,10" && b == "OK") || (call_ended(a) && b == "+CGEV: NW DEACT 8,10") {
            lines.swap(i, i + 1);
        }
    }
    lines
}

/// `VOICE CALL: END: HHMMSS` → seconds. The modem rounds down, so a call
/// ending a few ms later in the simulator can read one second longer.
fn duration(line: &str) -> Option<u32> {
    let d = line.strip_prefix("VOICE CALL: END: ")?;
    let n = |i: usize| d.get(i..i + 2)?.parse::<u32>().ok();
    Some(n(0)? * 3600 + n(2)? * 60 + n(4)?)
}

fn without_duration(line: &str) -> String {
    match duration(line) {
        Some(_) => "VOICE CALL: END: <duration>".into(),
        None => line.to_string(),
    }
}

fn real_lines(records: &[TraceRecord]) -> Vec<(Millis, String)> {
    timeline(records)
        .into_iter()
        .filter_map(|s| match s.step {
            Step::Line(l) => Some((s.t_ms, l)),
            _ => None,
        })
        .collect()
}

#[test]
fn simulator_reproduces_every_capture() {
    for (name, text) in ALL {
        let records = parse_trace(text).unwrap();
        let end = records.last().unwrap().t_ms + 1000;
        let real = normalise(real_lines(&records));
        let sim = normalise(run_sim(inputs(&records), end));
        let real_text: Vec<String> = real.iter().map(|(_, l)| without_duration(l)).collect();
        let sim_text: Vec<String> = sim.iter().map(|(_, l)| without_duration(l)).collect();
        if let Some(i) = (0..sim_text.len().max(real_text.len())).find(|&i| sim_text.get(i) != real_text.get(i)) {
            panic!(
                "{name}: first difference at line {i}\n  sim:  {:?}\n  real: {:?}",
                &sim[i.saturating_sub(2)..(i + 3).min(sim.len())],
                &real[i.saturating_sub(2)..(i + 3).min(real.len())]
            );
        }
        for ((ts, l), (tr, lr)) in sim.iter().zip(&real) {
            let tolerance = if l == NETWORK_TIMED { NETWORK_TIMED_TOLERANCE_MS } else { TOLERANCE_MS };
            assert!(ts.abs_diff(*tr) <= tolerance, "{name}: {l:?} at {ts} ms, real {tr} ms");
            if let (Some(ds), Some(dr)) = (duration(l), duration(lr)) {
                assert!(ds.abs_diff(dr) <= 1, "{name}: call duration {ds} s, real {dr} s");
            }
        }
    }
}

/// Guard against the replay passing vacuously: the inputs taken from the
/// traces must include each kind of network / far-end event.
#[test]
fn replay_exercises_every_kind_of_input() {
    let mut seen = std::collections::BTreeSet::new();
    for (_, text) in ALL {
        for (_, i) in inputs(&parse_trace(text).unwrap()) {
            seen.insert(format!("{i:?}").split('(').next().unwrap().to_string());
        }
    }
    for kind in [
        "Write", "PowerOn", "PowerOff", "CutPower", "Dialling", "Alerting", "Bearer", "RemoteAnswer",
        "RemoteHangUp", "IncomingCall", "ReceiveSms", "Stored", "NextMessageReference",
    ] {
        assert!(seen.contains(kind), "no {kind} input in any trace");
    }
}
