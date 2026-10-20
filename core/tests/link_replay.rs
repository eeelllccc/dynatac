//! Replays every captured trace through the L1 link engine
//! (`dynatac_core::modem::link`): the real modem's bytes, at their real
//! times, with each captured command submitted when it was sent.
//!
//! For every fixture the engine must:
//!   - write exactly the bytes the capture tool wrote, in order;
//!   - settle every command with the modem's actual final code
//!     (never `Unknown`), and never lose sync;
//!   - account for every received line as an echo, a reply line or a URC;
//!   - give replies that the L2 request parses.

use dynatac_core::modem::link::{
    probe_answer, Command, Engine, Failure, Info, LinkEvent, Outcome, Output, ReplySpec,
};
use dynatac_core::modem::request::{Gain, PhoneNumber, Request, Response};
use dynatac_core::modem::sms_pdu::encode_submit;
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
    fixture!("14-sms-receive-idle"),
    fixture!("15-sms-receive-in-call"),
    fixture!("16-sms-send"),
];

const NONE: ReplySpec = ReplySpec { info: Info::None, extra_finals: &[] };

/// The texts the capture tool sends (`device/examples/modem_capture.rs`).
const SMS_TEXTS: &[&str] = &[
    "dynatac test: short GSM text",
    "dynatac test: ünïcödé ☎ 👋 — long enough to need two parts as UCS2, which allows only 67 units per part with a header.",
];

/// The L2 request behind each captured command, so the replay checks the
/// real commands and reply specs. `AT+CLCC=1` (the HAL doesn't send it:
/// this firmware answers `ERROR`) and `AT+CMGD=1,4` (capture tool only)
/// have none. `payload` is what the capture tool wrote at the `> ` prompt.
fn request(text: &str, payload: Option<&[u8]>) -> Option<Request> {
    if let Some(len) = text.strip_prefix("AT+CMGS=") {
        let payload = payload.expect("CMGS payload");
        let pdu = SMS_TEXTS
            .iter()
            .flat_map(|t| encode_submit("07700900123", t, 1).unwrap())
            .find(|p| p.hex().as_bytes() == &payload[..payload.len() - 1])
            .expect("the payload is one of the capture tool's texts");
        assert_eq!(pdu.tpdu_len().to_string(), len);
        return Some(Request::SendMessage(pdu));
    }
    let index = |p: &str| text.strip_prefix(p).and_then(|i| i.parse().ok());
    Some(match text {
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
        "AT+CMGF=0" => Request::SetPduMode,
        "AT+CMGL=4" => Request::ListMessages,
        "AT+CLCC=1" | "AT+CMGD=1,4" => return None,
        t if index("AT+CMGR=").is_some() => Request::ReadMessage(index("AT+CMGR=").unwrap()),
        t if index("AT+CMGD=").is_some() => Request::DeleteMessage(index("AT+CMGD=").unwrap()),
        t => Request::Dial(
            PhoneNumber::parse(t.strip_prefix("ATD").and_then(|n| n.strip_suffix(';')).expect(t)).unwrap(),
        ),
    })
}

fn command(text: &str, payload: Option<&[u8]>) -> Command {
    match request(text, payload) {
        Some(r) => {
            let cmd = r.command();
            assert_eq!(cmd.text, text);
            cmd
        }
        None => Command { text: text.into(), spec: NONE, timeout_ms: 10_000, payload: None },
    }
}

fn is_payload(bytes: &[u8]) -> bool {
    bytes.last() == Some(&0x1A)
}

/// Engine time = trace time + this, leaving room to sync first.
const OFFSET: u64 = 1000;

struct Replay {
    writes: Vec<Vec<u8>>,
    replies: Vec<(String, Outcome, Vec<String>)>, // command, outcome, info lines
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
    for (i, rec) in records.iter().enumerate() {
        let now = OFFSET + rec.t_ms;
        e.on_tick(now);
        match rec.dir {
            // The engine writes the payload itself, at the prompt.
            Direction::Tx if is_payload(&rec.bytes) => {}
            Direction::Tx => {
                let text = String::from_utf8_lossy(&rec.bytes).trim_end_matches('\r').to_string();
                let next_tx = records[i + 1..].iter().find(|r| r.dir == Direction::Tx);
                let payload = next_tx.map(|r| r.bytes.as_slice()).filter(|b| is_payload(b));
                e.submit(command(&text, payload), now);
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
                    r.replies.push((cmd, rep.outcome, rep.info));
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
        let sent =
            parse_trace(text).unwrap().iter().filter(|x| x.dir == Direction::Tx && !is_payload(&x.bytes)).count();
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
        let info: usize = r.replies.iter().map(|(_, _, info)| info.len()).sum();
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

/// Every successful reply parses as its request's typed response.
#[test]
fn every_reply_parses_as_its_request() {
    for (name, text) in ALL {
        let records = parse_trace(text).unwrap();
        let payloads: Vec<&[u8]> =
            records.iter().filter(|r| r.dir == Direction::Tx && is_payload(&r.bytes)).map(|r| r.bytes.as_slice()).collect();
        let mut payloads = payloads.into_iter();
        for (cmd, outcome, info) in replay(text).replies {
            let payload = if cmd.starts_with("AT+CMGS=") { payloads.next() } else { None };
            let (Some(req), Outcome::Ok) = (request(&cmd, payload), &outcome) else { continue };
            assert!(req.parse(&info).is_ok(), "{name}: {cmd} {info:?}");
        }
    }
}

/// The SMS replies, typed: listings hold every stored message (a `+CMTI`
/// arriving meanwhile stays a URC), sends return the message reference.
#[test]
fn sms_replies_are_typed() {
    let typed = |name: &str| -> Vec<(String, Response)> {
        let (_, text) = ALL.iter().find(|(n, _)| *n == name).unwrap();
        let records = parse_trace(text).unwrap();
        let mut payloads = records.iter().filter(|r| r.dir == Direction::Tx && is_payload(&r.bytes));
        replay(text)
            .replies
            .into_iter()
            .filter(|(c, _, _)| c.starts_with("AT+CMG"))
            .filter_map(|(cmd, _, info)| {
                let payload = cmd.starts_with("AT+CMGS=").then(|| payloads.next().unwrap().bytes.as_slice());
                let req = request(&cmd, payload)?;
                Some((cmd, req.parse(&info).unwrap()))
            })
            .collect()
    };
    let count = |r: &Response| match r {
        Response::Messages(m) => m.iter().map(|m| m.index).collect::<Vec<_>>(),
        r => panic!("{r:?}"),
    };
    let t14 = typed("14-sms-receive-idle");
    let lists: Vec<_> = t14.iter().filter(|(c, _)| c == "AT+CMGL=4").map(|(_, r)| count(r)).collect();
    assert_eq!(lists, [vec![], vec![1, 2, 3, 4, 5]]);
    let t15 = typed("15-sms-receive-in-call");
    assert_eq!(count(&t15.last().unwrap().1), [1, 2, 3, 4, 5, 6]);
    let t16 = typed("16-sms-send");
    let refs: Vec<_> = t16
        .iter()
        .filter_map(|(_, r)| match r {
            Response::MessageRef(m) => Some(*m),
            _ => None,
        })
        .collect();
    assert_eq!(refs, [24, 25, 26]);
    let read = t16.iter().find(|(c, _)| c == "AT+CMGR=1").unwrap();
    assert_eq!(count(&read.1), [1]);
    assert_eq!(count(&t16.last().unwrap().1), [2, 3, 4, 5, 6]);
}
