//! Typed modem requests: what each AT command is, its reply format and
//! timeout, and how its reply is parsed.
//!
//! Each [`Request`] knows its exact command text ([`Request::command`]) and
//! turns its info lines into a typed [`Response`] ([`Request::parse`]).
//! Inputs that end up inside a command are validated types
//! ([`PhoneNumber`], [`Gain`]), so a request can never inject a second
//! command (`"123;ATH"`).
//!
//! Timeouts are the manual's "Max Response Time" (A76XX V1.09), with two
//! deliberate exceptions. `ATD` and `ATA` have none listed; the captures
//! show `OK` within 0.7 s, so 10 s. `ATE` / `ATI` are listed at 120 s (a
//! generic figure) but answer in 10–30 ms in every capture; at 120 s one
//! damaged final during configuration would stall the modem for two
//! minutes (found by fault testing), so they get the 9 s default.
//!
//! `ATD` / `ATA` end only on `OK` / `ERROR` / `+CME ERROR`, although the
//! manual also lists `NO CARRIER`, `BUSY` … as their results. On this modem
//! a voice `ATD` answers `OK` at once and call failure arrives as URCs (every
//! capture, incl. rejected / unreachable). Fault testing showed the danger:
//! the previous call's `NO CARRIER`, arriving just after a redial's `ATD`,
//! ended the `ATD` early and every later reply was credited one command
//! late. Mistaking a final for a URC only costs a timeout (`Unknown`);
//! the reverse corrupts replies.

use super::link::{Command, Info, Millis, ReplySpec};
use super::unsolicited::{parse_call, CallInfo};

/// A dialable number: an optional leading `+`, then 1–40 of `0-9 * #`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneNumber(String);

impl PhoneNumber {
    pub fn parse(s: &str) -> Option<Self> {
        let digits = s.strip_prefix('+').unwrap_or(s);
        let ok = (1..=40).contains(&digits.len())
            && digits.bytes().all(|b| b.is_ascii_digit() || b == b'*' || b == b'#');
        ok.then(|| PhoneNumber(s.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Speaker gain for `AT+COUTGAIN`, 0–7 (default 4). Calls must use 7
/// (see project notes: anything lower is too quiet on this board).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gain(u8);

impl Gain {
    pub const MAX: Gain = Gain(7);

    pub fn new(level: u8) -> Option<Self> {
        (level <= Self::MAX.0).then_some(Gain(level))
    }

    pub fn level(self) -> u8 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `ATD<n>;`: start a voice call. `Ok` means dialling started; progress
    /// arrives as `+CLCC` URCs.
    Dial(PhoneNumber),
    /// `ATA`: answer the ringing call.
    Answer,
    /// `AT+CHUP`: end all calls. Its `OK` arrives after the call-ended URCs.
    HangUp,
    /// `AT+CLCC`: list current calls (none → empty list).
    ListCalls,
    SetOutputGain(Gain),
    QueryOutputGain,
    /// `ATE0`.
    EchoOff,
    /// `AT+CMEE=2`: verbose `+CME ERROR` text.
    SetVerboseErrors,
    QueryVerboseErrors,
    /// `AT+CVHU=0`: make `ATH` hang up voice calls too.
    SetVoiceHangUp,
    QueryVoiceHangUp,
    /// `AT+CLIP=1`: caller ID with `RING`.
    SetCallerId,
    QueryCallerId,
    /// `AT+CURCD=0,1`: no URC delay (the resync probe leaves a small one).
    ClearUrcDelay,
    QueryUrcDelay,
    /// `ATI`: manufacturer, model, revision, IMEI.
    Identify,
    /// `AT+CGMR`.
    Firmware,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The command succeeded and has no data.
    Done,
    Calls(Vec<CallInfo>),
    Gain(Gain),
    /// A numeric setting read back (`+CMEE: 2`, `+CVHU: 0`, `+CLIP: 1,…`).
    Setting(u8),
    Lines(Vec<String>),
    Firmware(String),
}

const T_DEFAULT: Millis = 9_000;
const T_CALL: Millis = 10_000;

fn setting_line(prefix: &'static str) -> impl Fn(&str) -> Option<u8> {
    move |l: &str| l.strip_prefix(prefix)?.split(',').next()?.trim().parse().ok()
}

/// `ATI` reply lines (captured). Matching them exactly keeps an unknown URC
/// that lands mid-reply out of the response.
fn is_ati_line(l: &str) -> bool {
    ["Manufacturer:", "Model:", "Revision:", "IMEI:", "+GCAP:"].iter().any(|p| l.starts_with(p))
}

fn is_clip_setting(l: &str) -> bool {
    // `+CLIP: <n>,<m>` (all numeric), unlike the caller-ID URC.
    l.strip_prefix("+CLIP: ")
        .is_some_and(|r| r.split(',').all(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit())))
}

impl Request {
    pub fn command(&self) -> Command {
        use Request as R;
        let none = ReplySpec { info: Info::None, extra_finals: &[] };
        let prefixed = |p: &'static [&'static str]| ReplySpec { info: Info::Prefixes(p), extra_finals: &[] };
        let (text, spec, timeout_ms) = match self {
            R::Dial(n) => (format!("ATD{};", n.as_str()), none, T_CALL),
            R::Answer => ("ATA".into(), none, T_CALL),
            R::HangUp => ("AT+CHUP".into(), none, T_DEFAULT),
            R::ListCalls => ("AT+CLCC".into(), prefixed(&["+CLCC: "]), T_DEFAULT),
            R::SetOutputGain(g) => (format!("AT+COUTGAIN={}", g.level()), none, T_DEFAULT),
            R::QueryOutputGain => ("AT+COUTGAIN?".into(), prefixed(&["+COUTGAIN: "]), T_DEFAULT),
            R::EchoOff => ("ATE0".into(), none, T_DEFAULT),
            R::SetVerboseErrors => ("AT+CMEE=2".into(), none, T_DEFAULT),
            R::QueryVerboseErrors => ("AT+CMEE?".into(), prefixed(&["+CMEE: "]), T_DEFAULT),
            R::SetVoiceHangUp => ("AT+CVHU=0".into(), none, T_DEFAULT),
            R::QueryVoiceHangUp => ("AT+CVHU?".into(), prefixed(&["+CVHU: "]), T_DEFAULT),
            R::SetCallerId => ("AT+CLIP=1".into(), none, T_DEFAULT),
            R::QueryCallerId => (
                "AT+CLIP?".into(),
                ReplySpec { info: Info::Matching(is_clip_setting), extra_finals: &[] },
                T_DEFAULT,
            ),
            R::ClearUrcDelay => ("AT+CURCD=0,1".into(), none, T_DEFAULT),
            R::QueryUrcDelay => ("AT+CURCD?".into(), prefixed(&["+CURCD: "]), T_DEFAULT),
            R::Identify => ("ATI".into(), ReplySpec { info: Info::Matching(is_ati_line), extra_finals: &[] }, T_DEFAULT),
            R::Firmware => ("AT+CGMR".into(), prefixed(&["+CGMR: "]), T_DEFAULT),
        };
        Command { text, spec, timeout_ms, payload: None }
    }

    /// Turn a successful reply's info lines into a typed response. `Err`
    /// carries the lines when they don't have the expected shape.
    pub fn parse(&self, info: &[String]) -> Result<Response, Vec<String>> {
        use Request as R;
        let bad = || info.to_vec();
        let one = || match info {
            [l] => Ok(l.as_str()),
            _ => Err(bad()),
        };
        match self {
            R::Dial(_)
            | R::Answer
            | R::HangUp
            | R::SetOutputGain(_)
            | R::EchoOff
            | R::SetVerboseErrors
            | R::SetVoiceHangUp
            | R::SetCallerId
            | R::ClearUrcDelay => {
                if info.is_empty() {
                    Ok(Response::Done)
                } else {
                    Err(bad())
                }
            }
            // A `+CLCC` URC arriving during the query is taken as a reply line
            // (same format), so a call can appear twice: keep the latest line
            // per call index (later in the stream = more recent state).
            R::ListCalls => {
                let all = info
                    .iter()
                    .map(|l| l.strip_prefix("+CLCC: ").and_then(parse_call))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(bad)?;
                let mut calls: Vec<CallInfo> = Vec::new();
                for c in all {
                    match calls.iter_mut().find(|k| k.index == c.index) {
                        Some(k) => *k = c,
                        None => calls.push(c),
                    }
                }
                Ok(Response::Calls(calls))
            }
            R::QueryOutputGain => setting_line("+COUTGAIN: ")(one()?)
                .and_then(Gain::new)
                .map(Response::Gain)
                .ok_or_else(bad),
            R::QueryVerboseErrors => setting_line("+CMEE: ")(one()?).map(Response::Setting).ok_or_else(bad),
            R::QueryVoiceHangUp => setting_line("+CVHU: ")(one()?).map(Response::Setting).ok_or_else(bad),
            R::QueryCallerId => setting_line("+CLIP: ")(one()?).map(Response::Setting).ok_or_else(bad),
            R::QueryUrcDelay => setting_line("+CURCD: ")(one()?).map(Response::Setting).ok_or_else(bad),
            R::Identify => Ok(Response::Lines(info.to_vec())),
            R::Firmware => one()?
                .strip_prefix("+CGMR: ")
                .map(|v| Response::Firmware(v.to_string()))
                .ok_or_else(bad),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::unsolicited::{CallState, Direction};

    fn lines(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn phone_numbers_are_validated() {
        for ok in ["07700900123", "+447700900123", "*#06#", "1"] {
            assert!(PhoneNumber::parse(ok).is_some(), "{ok}");
        }
        for bad in ["", "+", "123;ATH", "07700 900123", "0770090012a", "++44", &"1".repeat(41)] {
            assert!(PhoneNumber::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn gain_is_zero_to_seven() {
        assert_eq!(Gain::new(7), Some(Gain::MAX));
        assert_eq!(Gain::new(0).map(Gain::level), Some(0));
        assert_eq!(Gain::new(8), None);
    }

    #[test]
    fn command_texts() {
        let n = PhoneNumber::parse("07700900123").unwrap();
        let cases = [
            (Request::Dial(n), "ATD07700900123;"),
            (Request::Answer, "ATA"),
            (Request::HangUp, "AT+CHUP"),
            (Request::ListCalls, "AT+CLCC"),
            (Request::SetOutputGain(Gain::MAX), "AT+COUTGAIN=7"),
            (Request::QueryOutputGain, "AT+COUTGAIN?"),
            (Request::EchoOff, "ATE0"),
            (Request::SetVerboseErrors, "AT+CMEE=2"),
            (Request::SetVoiceHangUp, "AT+CVHU=0"),
            (Request::SetCallerId, "AT+CLIP=1"),
            (Request::QueryCallerId, "AT+CLIP?"),
            (Request::ClearUrcDelay, "AT+CURCD=0,1"),
            (Request::QueryUrcDelay, "AT+CURCD?"),
            (Request::Identify, "ATI"),
            (Request::Firmware, "AT+CGMR"),
        ];
        for (r, text) in cases {
            assert_eq!(r.command().text, text);
        }
    }

    /// `NO CARRIER` / `BUSY` are URCs for every call command (see module
    /// docs: found by fault testing).
    #[test]
    fn call_commands_never_end_on_call_progress_codes() {
        let n = PhoneNumber::parse("07700900123").unwrap();
        for r in [Request::Dial(n), Request::Answer, Request::HangUp] {
            assert!(r.command().spec.extra_finals.is_empty(), "{r:?}");
        }
    }

    #[test]
    fn list_calls_parses_each_line() {
        let r = Request::ListCalls.parse(&lines(&["+CLCC: 1,1,4,0,0,\"07700900123\",129,\"\""])).unwrap();
        let Response::Calls(calls) = r else { panic!() };
        assert_eq!(calls.len(), 1);
        assert_eq!((calls[0].direction, calls[0].state), (Direction::Incoming, CallState::Incoming));
        assert_eq!(Request::ListCalls.parse(&[]), Ok(Response::Calls(vec![])));
        assert!(Request::ListCalls.parse(&lines(&["+CLCC: junk"])).is_err());
    }

    /// Found by fault testing: a `+CLCC` URC absorbed into the reply listed
    /// the call twice.
    #[test]
    fn list_calls_keeps_the_latest_line_per_call() {
        let r = Request::ListCalls
            .parse(&lines(&[
                "+CLCC: 1,0,2,0,0,\"07700900123\",129,\"\"",
                "+CLCC: 1,0,3,0,0,\"07700900123\",129,\"\"",
            ]))
            .unwrap();
        let Response::Calls(calls) = r else { panic!() };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, CallState::Alerting);
    }

    /// Found by fault testing: an unknown URC inside the `ATI` reply was
    /// taken as part of it.
    #[test]
    fn identify_only_takes_ati_lines() {
        let Info::Matching(f) = Request::Identify.command().spec.info else { panic!("not a predicate") };
        assert!(f("Manufacturer: INCORPORATED") && f("+GCAP: +CGSM,+FCLASS,+DS") && f("IMEI: 000000000000000"));
        assert!(!f("JUNK122") && !f("RING"));
    }

    #[test]
    fn settings_read_back() {
        assert_eq!(Request::QueryOutputGain.parse(&lines(&["+COUTGAIN: 7"])), Ok(Response::Gain(Gain::MAX)));
        assert!(Request::QueryOutputGain.parse(&lines(&["+COUTGAIN: 9"])).is_err());
        assert_eq!(Request::QueryVerboseErrors.parse(&lines(&["+CMEE: 2"])), Ok(Response::Setting(2)));
        assert_eq!(Request::QueryVoiceHangUp.parse(&lines(&["+CVHU: 0"])), Ok(Response::Setting(0)));
        assert_eq!(Request::QueryCallerId.parse(&lines(&["+CLIP: 1,1"])), Ok(Response::Setting(1)));
        assert_eq!(Request::QueryUrcDelay.parse(&lines(&["+CURCD: 0,1"])), Ok(Response::Setting(0)));
        assert!(Request::QueryCallerId.parse(&[]).is_err());
    }

    #[test]
    fn clip_reply_predicate_rejects_the_caller_id_urc() {
        assert!(is_clip_setting("+CLIP: 1,1"));
        assert!(is_clip_setting("+CLIP: 0,2"));
        assert!(!is_clip_setting("+CLIP: \"07700900123\",129"));
        assert!(!is_clip_setting("+CLIP: ,1"));
    }

    #[test]
    fn identity_and_firmware() {
        let ati = lines(&["Manufacturer: INCORPORATED", "Model: A7682E"]);
        assert_eq!(Request::Identify.parse(&ati), Ok(Response::Lines(ati.clone())));
        assert_eq!(
            Request::Firmware.parse(&lines(&["+CGMR: A011B15A7682M7"])),
            Ok(Response::Firmware("A011B15A7682M7".into()))
        );
    }

    #[test]
    fn unexpected_info_on_a_simple_command_is_an_error() {
        assert_eq!(Request::HangUp.parse(&lines(&["junk"])), Err(lines(&["junk"])));
    }
}
