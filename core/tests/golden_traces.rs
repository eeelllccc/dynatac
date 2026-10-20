//! Facts about the real A7682E, checked against traces captured on the
//! T-Deck-Pro (`captures/`, turned into fixtures by
//! `captures/make-fixtures.sh`). Firmware `A7682M7_V1.11.1`.
//!
//! Each test pins one behaviour the modem stack relies on, so the design
//! rests on evidence rather than on the manual alone. The modem simulator
//! must later reproduce all of these.
//!
//! Fixture conventions: the dialled number is `07700900123`. Notes are
//! capture-tool events (`pwrkey on-pulse end`, `ri low`, `mark: …`).

use dynatac_core::modem::trace::{parse_trace, timeline, Step, TimedStep};

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

const DIAL: &str = "ATD07700900123;";

// --- helpers ------------------------------------------------------------------

fn load(name: &str) -> Vec<TimedStep> {
    let (_, text) = ALL.iter().find(|(n, _)| *n == name).expect("unknown fixture");
    timeline(&parse_trace(text).expect("fixture parses"))
}

fn is_final(line: &str) -> bool {
    line == "OK" || line == "ERROR" || line.starts_with("+CME ERROR") || line.starts_with("+CMS ERROR")
}

/// Every reply to `cmd`: the lines after each send of it, up to and
/// including `OK` / `ERROR` / `+CME|CMS ERROR`. Includes any URCs that
/// arrived in between — that is the point of several tests.
fn replies(steps: &[TimedStep], cmd: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for s in steps {
        match &s.step {
            Step::Sent(c) => {
                assert!(current.is_none(), "{cmd}: sent {c} before the final code");
                if c == cmd {
                    current = Some(Vec::new());
                }
            }
            Step::Line(l) => {
                if let Some(lines) = current.as_mut() {
                    lines.push(l.clone());
                    if is_final(l) {
                        out.push(current.take().unwrap());
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Lines received outside any reply to `cmd` (e.g. call URCs with the
/// answers to `AT+CLCC` queries removed).
fn lines_outside_replies(steps: &[TimedStep], cmd: &str) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    let mut in_reply = false;
    for s in steps {
        match &s.step {
            Step::Sent(c) => in_reply = c == cmd,
            Step::Line(l) if in_reply => in_reply = !is_final(l),
            Step::Line(l) => out.push((s.t_ms, l.clone())),
            _ => {}
        }
    }
    out
}

/// A reply without the command's echo (present until `ATE0`).
fn without_echo(reply: Vec<String>, cmd: &str) -> Vec<String> {
    reply.into_iter().filter(|l| l != cmd).collect()
}

fn reply(steps: &[TimedStep], cmd: &str) -> Vec<String> {
    replies(steps, cmd).into_iter().next().unwrap_or_else(|| panic!("no reply to {cmd}"))
}

/// A line with the variable parts replaced, so sequences compare across
/// calls: `+CLCC: 1,0,6,…` → `+CLCC stat 6`, `VOICE CALL: END: 000035` →
/// `VOICE CALL: END: <duration>`.
fn shape(line: &str) -> String {
    if let Some(rest) = line.strip_prefix("+CLCC: ") {
        let stat = rest.split(',').nth(2).unwrap_or("?");
        return format!("+CLCC stat {stat}");
    }
    if line.starts_with("VOICE CALL: END: ") {
        return "VOICE CALL: END: <duration>".into();
    }
    line.to_string()
}

fn shapes(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| shape(l)).collect()
}

fn lines(steps: &[TimedStep]) -> Vec<(u64, String)> {
    steps
        .iter()
        .filter_map(|s| match &s.step {
            Step::Line(l) => Some((s.t_ms, l.clone())),
            _ => None,
        })
        .collect()
}

fn note_times(steps: &[TimedStep], note: &str) -> Vec<u64> {
    steps
        .iter()
        .filter(|s| s.step == Step::Note(note.into()))
        .map(|s| s.t_ms)
        .collect()
}

fn note_time(steps: &[TimedStep], note: &str) -> u64 {
    *note_times(steps, note).first().unwrap_or_else(|| panic!("no note {note:?}"))
}

fn lines_between(steps: &[TimedStep], from: u64, to: u64) -> Vec<String> {
    lines(steps).into_iter().filter(|(t, _)| *t > from && *t < to).map(|(_, l)| l).collect()
}

/// (low_at, high_at) for each RI low pulse.
fn ri_pulses(steps: &[TimedStep]) -> Vec<(u64, u64)> {
    let lows = note_times(steps, "ri low");
    let highs = note_times(steps, "ri high");
    lows.iter()
        .map(|&lo| (lo, *highs.iter().find(|&&h| h > lo).expect("ri goes high again")))
        .collect()
}

const OUTGOING: &[&str] = &[
    "02-out-answered-remote-hangup",
    "03-out-answered-voicemail-hangup",
    "03-out-answered-local-hangup",
    "04-out-rejected",
    "05-out-unanswered",
    "06-out-busy-or-unreachable",
    "09-sms-during-call",
    "12-command-during-urc",
];

const INITIALISED: &[&str] = &[
    "03-out-answered-voicemail-hangup",
    "03-out-answered-local-hangup",
    "04-out-rejected",
    "05-out-unanswered",
    "06-out-busy-or-unreachable",
    "07-in-answered",
    "08-in-missed",
    "09-sms-during-call",
    "10-soft-power-cycle",
    "11-hard-power-cut",
    "12-command-during-urc",
];

const BOOT: &[&str] = &["*ATREADY: 1", "+CPIN: READY", "SMS DONE", "+CGEV: EPS PDN ACT 1", "PB DONE"];

// --- fixtures -----------------------------------------------------------------

#[test]
fn every_fixture_parses_and_contains_modem_output() {
    for (name, _) in ALL {
        let steps = load(name);
        assert!(lines(&steps).len() >= BOOT.len(), "{name}");
    }
}

// --- boot, power, restart -----------------------------------------------------

/// `*ATREADY: 1` starts every boot, ~6 s after PWRKEY is released, and the
/// rest of the boot output follows in a fixed order. It is how a restart
/// (and so the loss of every `NO_SAVE` setting) is detected.
#[test]
fn boot_output_is_identical_after_every_power_up() {
    let mut boots = 0;
    for name in ["01-boot", "10-soft-power-cycle", "11-hard-power-cut"] {
        let steps = load(name);
        for released in note_times(&steps, "pwrkey on-pulse end") {
            let after: Vec<_> =
                lines(&steps).into_iter().filter(|(t, _)| *t > released).take(BOOT.len()).collect();
            let texts: Vec<_> = after.iter().map(|(_, l)| l.as_str()).collect();
            assert_eq!(texts, BOOT, "{name} @ {released}");
            let ready_after = after[0].0 - released;
            assert!((5800..=6300).contains(&ready_after), "{name}: ATREADY after {ready_after} ms");
            boots += 1;
        }
    }
    assert_eq!(boots, 5);
}

/// A soft power-off announces nothing but a network detach; there is no
/// `NORMAL POWER DOWN`. Power loss can only be detected by silence.
#[test]
fn soft_power_off_only_reports_detach() {
    let steps = load("10-soft-power-cycle");
    let off = note_time(&steps, "pwrkey off-pulse start");
    let on = note_times(&steps, "pwrkey on-pulse start")[1];
    assert_eq!(lines_between(&steps, off, on), ["+CGEV: ME DETACH"]);
}

#[test]
fn hard_power_cut_emits_nothing() {
    let steps = load("11-hard-power-cut");
    let cut = note_time(&steps, "power_en low");
    let released = note_times(&steps, "pwrkey on-pulse end")[1];
    assert_eq!(lines_between(&steps, cut, released), Vec::<String>::new());
}

/// SMS DONE at boot pulses RI low for ~110 ms.
#[test]
fn sms_done_pulses_ri() {
    for name in ["01-boot", "10-soft-power-cycle", "11-hard-power-cut"] {
        let steps = load(name);
        for (lo, hi) in ri_pulses(&steps) {
            assert!((80..=140).contains(&(hi - lo)), "{name}: RI low {} ms", hi - lo);
        }
    }
}

// --- configuration ------------------------------------------------------------

#[test]
fn echo_off_command_is_itself_echoed() {
    for name in INITIALISED {
        assert_eq!(reply(&load(name), "ATE0"), ["ATE0", "OK"], "{name}");
    }
}

/// `AT+CLCC=1` is rejected on this firmware, yet `+CLCC:` URCs are sent
/// anyway (02 never sent the command). `configure()` must not treat the
/// error as fatal, and must not depend on it.
#[test]
fn call_list_reporting_command_errors_but_reports_by_default() {
    for name in INITIALISED {
        assert_eq!(reply(&load(name), "AT+CLCC=1"), ["ERROR"], "{name}");
    }
    let steps = load("02-out-answered-remote-hangup");
    assert!(replies(&steps, "AT+CLCC=1").is_empty());
    assert!(lines(&steps).iter().any(|(_, l)| l.starts_with("+CLCC: ")));
}

// --- outgoing calls -------------------------------------------------------------

/// Dialling → alerting → connected, the connect burst always ordered
/// `VOICE CALL: BEGIN`, `+CLCC` stat 0, then an unrequested `+COLP`.
/// Rejected, unanswered and unreachable calls (04–06) all reached voicemail,
/// so they look identical: `BUSY` / `NO ANSWER` were never observed.
#[test]
fn outgoing_call_progresses_dialling_alerting_connected() {
    for name in OUTGOING {
        let steps = load(name);
        let call_lines: Vec<_> = lines_outside_replies(&steps, "AT+CLCC")
            .into_iter()
            .map(|(_, l)| shape(&l))
            .filter(|l| l.starts_with("+CLCC") || l.starts_with("VOICE CALL") || l.starts_with("+COLP"))
            .take(5)
            .collect();
        assert_eq!(
            call_lines,
            [
                "+CLCC stat 2",
                "+CLCC stat 3",
                "VOICE CALL: BEGIN",
                "+CLCC stat 0",
                "+COLP: \"07700900123\",129"
            ],
            "{name}"
        );
    }
}

/// The VoLTE bearer URC races `ATD`'s `OK`: sometimes inside the reply,
/// sometimes after. The link layer must accept URCs before a final code.
#[test]
fn dial_reply_sometimes_contains_a_urc() {
    let mut inside = 0;
    let mut after = 0;
    for name in OUTGOING {
        match without_echo(reply(&load(name), DIAL), DIAL).as_slice() {
            [ok] if ok == "OK" => after += 1,
            [urc, ok] if urc == "+CGEV: NW ACT 8,10" && ok == "OK" => inside += 1,
            other => panic!("{name}: unexpected ATD reply {other:?}"),
        }
    }
    assert!(inside > 0 && after > 0, "inside={inside} after={after}");
}

// --- hang-up ------------------------------------------------------------------

/// `AT+CHUP`'s `OK` arrives *after* the call-ended URCs, including
/// `NO CARRIER`. So `NO CARRIER` is not a final code for `CHUP`; treating
/// it as one releases the link early and mis-credits the late `OK`.
#[test]
fn hang_up_ok_arrives_after_no_carrier() {
    for name in [
        "03-out-answered-local-hangup",
        "04-out-rejected",
        "05-out-unanswered",
        "06-out-busy-or-unreachable",
        "09-sms-during-call",
    ] {
        assert_eq!(
            shapes(&reply(&load(name), "AT+CHUP")),
            [
                "+CGEV: NW DEACT 8,10",
                "+CLCC stat 6",
                "VOICE CALL: END: <duration>",
                "NO CARRIER",
                "OK"
            ],
            "{name}"
        );
    }
}

#[test]
fn remote_hang_up_sequence() {
    for name in [
        "02-out-answered-remote-hangup",
        "03-out-answered-voicemail-hangup",
        "07-in-answered",
        "12-command-during-urc",
    ] {
        let all: Vec<_> = lines(&load(name)).into_iter().map(|(_, l)| l).collect();
        let end = all.iter().position(|l| l == "NO CARRIER").expect("call ends");
        assert_eq!(
            shapes(&all[end - 3..=end]),
            ["+CGEV: NW DEACT 8,10", "+CLCC stat 6", "VOICE CALL: END: <duration>", "NO CARRIER"],
            "{name}"
        );
    }
}

/// With no call, `AT+CLCC` is a bare `OK`.
#[test]
fn call_list_is_empty_after_hang_up() {
    for name in [
        "02-out-answered-remote-hangup",
        "03-out-answered-local-hangup",
        "04-out-rejected",
        "08-in-missed",
    ] {
        let last = replies(&load(name), "AT+CLCC").pop().unwrap();
        assert_eq!(without_echo(last, "AT+CLCC"), ["OK"], "{name}");
    }
}

// --- incoming calls -------------------------------------------------------------

/// First ring: `RING`, `+CLIP`, `+CLCC` (dir 1 = incoming, stat 4). Later
/// rings: `RING` + `+CLIP` only, every 6.0 s.
#[test]
fn incoming_call_rings_every_six_seconds_with_caller_id() {
    for name in ["07-in-answered", "08-in-missed"] {
        let all = lines(&load(name));
        let first = all.iter().position(|(_, l)| l == "RING").unwrap();
        let texts: Vec<_> = all[first..first + 3].iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(
            texts,
            ["RING", "+CLIP: \"07700900123\",129", "+CLCC: 1,1,4,0,0,\"07700900123\",129,\"\""],
            "{name}"
        );
        let rings: Vec<_> = all.iter().filter(|(_, l)| l == "RING").map(|(t, _)| *t).collect();
        assert!(rings.len() >= 3, "{name}");
        for pair in rings.windows(2) {
            let gap = pair[1] - pair[0];
            assert!((5950..=6050).contains(&gap), "{name}: ring gap {gap} ms");
        }
        for (i, (_, l)) in all.iter().enumerate().skip(first + 3) {
            if l == "RING" {
                assert_eq!(all[i + 1].1, "+CLIP: \"07700900123\",129", "{name}");
            }
        }
    }
}

/// `ATA`'s `OK` arrives after the call-connected URCs.
#[test]
fn answer_ok_arrives_after_voice_call_begin() {
    assert_eq!(
        shapes(&reply(&load("07-in-answered"), "ATA")),
        ["+CGEV: NW ACT 8,10", "VOICE CALL: BEGIN", "+CLCC stat 0", "OK"]
    );
}

/// A missed call ends with `VOICE CALL: END` *without* a duration, and no
/// bearer URCs (none was set up).
#[test]
fn missed_call_ends_without_duration() {
    let steps = load("08-in-missed");
    let all: Vec<_> = lines(&steps).into_iter().map(|(_, l)| l).collect();
    let end = all.iter().position(|l| l == "NO CARRIER").unwrap();
    assert_eq!(shapes(&all[end - 2..=end]), ["+CLCC stat 6", "VOICE CALL: END", "NO CARRIER"]);
    let incoming = note_time(&steps, "mark: incoming");
    assert!(!lines_between(&steps, incoming, u64::MAX).iter().any(|l| l.starts_with("+CGEV")));
}

/// RI goes low with each `RING` (within a few ms, either side) and stays
/// low ~5.9 s. When the caller gives up, RI is released at once.
#[test]
fn ri_is_held_low_while_ringing() {
    for name in ["07-in-answered", "08-in-missed"] {
        let steps = load(name);
        let incoming = note_time(&steps, "mark: incoming");
        let pulses: Vec<_> = ri_pulses(&steps).into_iter().filter(|(lo, _)| *lo > incoming).collect();
        let rings: Vec<_> =
            lines(&steps).into_iter().filter(|(_, l)| l == "RING").map(|(t, _)| t).collect();
        assert_eq!(pulses.len(), rings.len(), "{name}");
        for (&(lo, _), &ring) in pulses.iter().zip(&rings) {
            assert!(lo.abs_diff(ring) <= 10, "{name}: RI low {lo}, RING {ring}");
        }
        let full = if name == "08-in-missed" { &pulses[..pulses.len() - 1] } else { &pulses[..] };
        for (lo, hi) in full {
            assert!((5850..=5950).contains(&(hi - lo)), "{name}: RI low {} ms", hi - lo);
        }
    }
    let steps = load("08-in-missed");
    let (_, released) = *ri_pulses(&steps).last().unwrap();
    let ended = lines(&steps).into_iter().find(|(_, l)| l == "NO CARRIER").unwrap().0;
    assert!(released.abs_diff(ended) <= 10);
}

// --- concurrency --------------------------------------------------------------

/// 30+ `AT+CLCC`s fired around connect and hang-up: every reply held only
/// `+CLCC` lines and `OK`. Call URCs landed between commands, never inside
/// a `CLCC` reply (unlike `ATD`, `ATA` and `CHUP` replies above).
#[test]
fn call_list_replies_contain_only_call_lines() {
    let all = replies(&load("12-command-during-urc"), "AT+CLCC");
    assert!(all.len() >= 30);
    for r in &all {
        let (last, info) = r.split_last().unwrap();
        assert_eq!(last, "OK");
        assert!(info.iter().all(|l| l.starts_with("+CLCC: ")), "{r:?}");
    }
}
