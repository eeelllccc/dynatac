//! Replays every captured trace through the L1 link engine
//! (`dynatac_core::modem::link`): the real modem's bytes, at their real
//! times, with each captured command submitted when it was sent.
//!
//! For every fixture the engine must:
//!   - write exactly the bytes the capture tool wrote, in order;
//!   - settle every command with the modem's actual final code
//!     (never `Unknown`), and never lose sync;
//!   - account for every received line as an echo, a reply line or a URC.

use dynatac_core::modem::link::{
    probe_answer, Command, Engine, Failure, Info, LinkEvent, Outcome, Output, ReplySpec,
};
use dynatac_core::modem::request::{Gain, PhoneNumber, Request};
use dynatac_core::modem::trace::{parse_trace, timeline, Direction, Step};
use dynatac_core::modem::unsolicited::is_urc;
use dynatac_core::modem::transport::Rx;

macro_rules! fixture {
    ($name:literal) => {
        ($name, include_str!(concat!("fixtures/modem/", $name, ".trace")))
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
];

const NONE: ReplySpec = ReplySpec { info: Info::None, extra_finals: &[] };

/// The L2 request behind each captured command, so the replay checks the
/// real reply specs. `AT+CLCC=1` has no request (the HAL doesn't send it:
/// this firmware answers `ERROR`), so it gets a plain spec.
fn spec(text: &str) -> ReplySpec {
    let request = match text {
        "ATE0" => Request::EchoOff,
        "AT+CMEE=2" => Request::SetVerboseErrors,
        "AT+CVHU=0" => Request::SetVoiceHangUp,
        "AT+CLIP=1" => Request::SetCallerId,
        "AT+COUTGAIN=7" => Request::SetOutputGain(Gain::MAX),
        "ATI" => Request::Identify,
        "AT+CGMR" => Request::Firmware,
        "ATA" => Request::Answer,
        "AT+CHUP" => Request::HangUp,
        "AT+CLCC" => Request::ListCalls,
        "AT+CLCC=1" => return NONE,
        t => Request::Dial(
            PhoneNumber::parse(t.strip_prefix("ATD").and_then(|n| n.strip_suffix(';')).expect(t)).unwrap(),
        ),
    };
    let cmd = request.command();
    assert_eq!(cmd.text, text);
    cmd.spec
}

/// Engine time = trace time + this, leaving room to sync first.
const OFFSET: u64 = 1000;

struct Replay {
    writes: Vec<Vec<u8>>,
    replies: Vec<(String, Outcome, usize)>, // command, outcome, info line count
    urcs: Vec<String>,
    events: Vec<LinkEvent>,
    /// Outputs in order, for checking relative order of URCs and replies.
    log: Vec<String>,
}

fn replay(text: &str) -> Replay {
    let records = parse_trace(text).unwrap();
    let mut e = Engine::new(is_urc, 0);
    let Some(Output::Write(probe)) = e.poll() else { panic!("engine starts by probing") };
    let answer = probe_answer(String::from_utf8_lossy(&probe).trim_end_matches('\r')).unwrap();
    e.on_rx(Rx::Data(answer.into_bytes()), 0);
    while e.poll().is_some() {} // Synced

    let mut r = Replay { writes: vec![], replies: vec![], urcs: vec![], events: vec![], log: vec![] };
    let mut sent: Vec<String> = Vec::new();
    for rec in &records {
        let now = OFFSET + rec.t_ms;
        e.on_tick(now);
        match rec.dir {
            Direction::Tx => {
                let text = String::from_utf8_lossy(&rec.bytes).trim_end_matches('\r').to_string();
                e.submit(Command { text: text.clone(), spec: spec(&text), timeout_ms: 10_000, payload: None }, now);
                sent.push(text);
            }
            Direction::Rx => e.on_rx(Rx::Data(rec.bytes.clone()), now),
            Direction::Note => {}
        }
        while let Some(o) = e.poll() {
            match o {
                Output::Write(b) => r.writes.push(b),
                Output::Reply(rep) => {
                    let cmd = sent[rep.id.0 as usize].clone();
                    r.log.push(format!("reply {cmd} {:?}", rep.outcome));
                    r.replies.push((cmd, rep.outcome, rep.info.len()));
                }
                Output::Urc(u) => {
                    r.log.push(format!("urc {u}"));
                    r.urcs.push(u);
                }
                Output::Event(ev) => r.events.push(ev),
            }
        }
    }
    r
}

#[test]
fn engine_writes_exactly_what_was_captured() {
    for (name, text) in ALL {
        let records = parse_trace(text).unwrap();
        let captured: Vec<Vec<u8>> =
            records.iter().filter(|r| r.dir == Direction::Tx).map(|r| r.bytes.clone()).collect();
        assert_eq!(replay(text).writes, captured, "{name}");
    }
}

#[test]
fn every_command_settles_with_the_modems_final_code() {
    for (name, text) in ALL {
        let r = replay(text);
        let sent = parse_trace(text).unwrap().iter().filter(|x| x.dir == Direction::Tx).count();
        assert_eq!(r.replies.len(), sent, "{name}: one reply per command");
        for (cmd, outcome, _) in &r.replies {
            let want = if cmd == "AT+CLCC=1" { Outcome::Failed(Failure::Error) } else { Outcome::Ok };
            assert_eq!(outcome, &want, "{name}: {cmd}");
        }
        assert_eq!(r.events, [], "{name}: link events");
    }
}

#[test]
fn every_received_line_is_accounted_for() {
    for (name, text) in ALL {
        let steps = timeline(&parse_trace(text).unwrap());
        let sent: Vec<&str> = steps
            .iter()
            .filter_map(|s| if let Step::Sent(c) = &s.step { Some(c.as_str()) } else { None })
            .collect();
        let lines: Vec<&str> = steps
            .iter()
            .filter_map(|s| if let Step::Line(l) = &s.step { Some(l.as_str()) } else { None })
            .collect();
        let echoes = lines.iter().filter(|l| sent.contains(l)).count();
        let r = replay(text);
        let info: usize = r.replies.iter().map(|(_, _, n)| n).sum();
        let finals = r.replies.len();
        assert_eq!(lines.len(), echoes + info + finals + r.urcs.len(), "{name}");
    }
}

#[test]
fn no_carrier_during_hang_up_is_delivered_as_a_urc_before_the_reply() {
    for name in [
        "03-out-answered-local-hangup",
        "04-out-rejected",
        "05-out-unanswered",
        "06-out-busy-or-unreachable",
        "09-sms-during-call",
    ] {
        let (_, text) = ALL.iter().find(|(n, _)| *n == name).unwrap();
        let log = replay(text).log;
        let reply = log.iter().position(|l| l == "reply AT+CHUP Ok").expect(name);
        let urc = log.iter().position(|l| l == "urc NO CARRIER").expect(name);
        assert!(urc < reply, "{name}: {log:#?}");
    }
}

#[test]
fn call_state_urcs_inside_the_answer_reply_stay_urcs() {
    let (_, text) = ALL.iter().find(|(n, _)| *n == "07-in-answered").unwrap();
    let log = replay(text).log;
    let reply = log.iter().position(|l| l == "reply ATA Ok").unwrap();
    assert_eq!(
        log[reply - 3..reply],
        ["urc +CGEV: NW ACT 8,10", "urc VOICE CALL: BEGIN", "urc +CLCC: 1,1,0,0,0,\"07700900123\",129,\"\""]
    );
}

/// Every URC the real modem sent in the captures is one L2 recognises.
#[test]
fn every_captured_urc_is_recognised() {
    for (name, text) in ALL {
        for u in replay(text).urcs {
            assert!(is_urc(&u), "{name}: unrecognised URC {u:?}");
        }
    }
}
