//! Typed URCs (unsolicited result codes) from the A7682E.
//!
//! [`parse`] turns one line into an [`Unsolicited`]. Every format here is
//! taken from the captured traces (`core/tests/fixtures/modem/`) or, where
//! not captured, from the A76XX AT manual V1.09. Lines that are not a known
//! URC come back as [`Unsolicited::Unrecognised`], never dropped.
//!
//! [`is_urc`] is the predicate the link engine uses to keep URCs out of
//! free-form replies (`ATI`).

/// One call, as reported by `+CLCC` (query reply and URC share the format).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallInfo {
    /// Call index (`<id1>`), stable for the call's lifetime.
    pub index: u8,
    pub direction: Direction,
    pub state: CallState,
    pub mode: CallMode,
    pub multiparty: bool,
    /// `None` when absent or withheld (empty).
    pub number: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Mobile originated: we dialled.
    Outgoing,
    /// Mobile terminated: they called.
    Incoming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    Active,
    Held,
    Dialling,
    Alerting,
    Incoming,
    Waiting,
    /// The call is ending; it will be absent from the next `AT+CLCC`.
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallMode {
    Voice,
    Data,
    Fax,
    Other(u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsolicited {
    /// `*ATREADY: 1`: the modem has (re)booted. Every `NO_SAVE` setting is
    /// back to its default.
    Ready,
    /// `+CPIN: <code>` (`READY`, `SIM PIN`, …).
    Sim(String),
    /// `SMS DONE`: SMS service initialised after boot.
    SmsReady,
    /// `PB DONE`: phonebook initialised after boot.
    PhonebookReady,
    /// `RING`: incoming call, repeated every ~6 s while ringing.
    Ring,
    /// `+CLIP: "<number>",<type>…`: caller of the ringing call.
    CallerId { number: Option<String> },
    /// `+COLP: "<number>",<type>`: the line we connected to.
    ConnectedLine { number: Option<String> },
    /// `+CLCC: …`: a call changed state.
    Call(CallInfo),
    /// `VOICE CALL: BEGIN`.
    VoiceCallBegin,
    /// `VOICE CALL: END[: HHMMSS]`. No duration for a missed call.
    VoiceCallEnd { duration_s: Option<u32> },
    /// `NO CARRIER`: the call is gone (remote or local hang-up, missed call).
    NoCarrier,
    Busy,
    NoAnswer,
    /// `+CMTI: "<mem>",<index>`: a new SMS was stored.
    NewSms { storage: String, index: u32 },
    /// `+CGEV: …`: packet-domain event (bearers, detach). Kept as text.
    PacketDomain(String),
    /// Any other line (including a fragment after lost bytes).
    Unrecognised(String),
}

/// Whether `line` is a known URC.
pub fn is_urc(line: &str) -> bool {
    !matches!(parse(line), Unsolicited::Unrecognised(_))
}

pub fn parse(line: &str) -> Unsolicited {
    use Unsolicited as U;
    let unrecognised = || U::Unrecognised(line.to_string());
    match line {
        "*ATREADY: 1" => return U::Ready,
        "SMS DONE" => return U::SmsReady,
        "PB DONE" => return U::PhonebookReady,
        "RING" => return U::Ring,
        "VOICE CALL: BEGIN" => return U::VoiceCallBegin,
        "VOICE CALL: END" => return U::VoiceCallEnd { duration_s: None },
        "NO CARRIER" => return U::NoCarrier,
        "BUSY" => return U::Busy,
        "NO ANSWER" => return U::NoAnswer,
        _ => {}
    }
    if let Some(d) = line.strip_prefix("VOICE CALL: END: ") {
        return parse_hhmmss(d).map_or_else(unrecognised, |s| U::VoiceCallEnd { duration_s: Some(s) });
    }
    if let Some(rest) = line.strip_prefix("+CLCC: ") {
        return parse_call(rest).map_or_else(unrecognised, U::Call);
    }
    if let Some(rest) = line.strip_prefix("+CLIP: ") {
        return parse_number_field(rest).map_or_else(unrecognised, |number| U::CallerId { number });
    }
    if let Some(rest) = line.strip_prefix("+COLP: ") {
        return parse_number_field(rest).map_or_else(unrecognised, |number| U::ConnectedLine { number });
    }
    if let Some(rest) = line.strip_prefix("+CMTI: ") {
        let mut f = fields(rest).into_iter();
        return match (f.next(), f.next().and_then(|i| i.parse().ok()), f.next()) {
            (Some(storage), Some(index), None) => U::NewSms { storage, index },
            _ => unrecognised(),
        };
    }
    if let Some(rest) = line.strip_prefix("+CPIN: ") {
        return U::Sim(rest.to_string());
    }
    if let Some(rest) = line.strip_prefix("+CGEV: ") {
        return U::PacketDomain(rest.to_string());
    }
    unrecognised()
}

/// `+CLCC: ` payload: `<id>,<dir>,<stat>,<mode>,<mpty>[,"<number>",<type>[,"<alpha>"]]`.
pub fn parse_call(rest: &str) -> Option<CallInfo> {
    let f = fields(rest);
    if f.len() < 5 {
        return None;
    }
    let num = |i: usize| f[i].parse::<u8>().ok();
    let direction = match num(1)? {
        0 => Direction::Outgoing,
        1 => Direction::Incoming,
        _ => return None,
    };
    let state = match num(2)? {
        0 => CallState::Active,
        1 => CallState::Held,
        2 => CallState::Dialling,
        3 => CallState::Alerting,
        4 => CallState::Incoming,
        5 => CallState::Waiting,
        6 => CallState::Disconnected,
        _ => return None,
    };
    let mode = match num(3)? {
        0 => CallMode::Voice,
        1 => CallMode::Data,
        2 => CallMode::Fax,
        m => CallMode::Other(m),
    };
    let multiparty = match num(4)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let number = f.get(5).filter(|n| !n.is_empty()).cloned();
    Some(CallInfo { index: num(0)?, direction, state, mode, multiparty, number })
}

/// First field as a phone number (quoted, may be empty); requires at least
/// a type field after it, as `+CLIP` / `+COLP` always have one.
fn parse_number_field(rest: &str) -> Option<Option<String>> {
    if !rest.starts_with('"') {
        return None;
    }
    let f = fields(rest);
    if f.len() < 2 {
        return None;
    }
    Some(Some(f[0].clone()).filter(|n| !n.is_empty()))
}

/// `HHMMSS` → seconds.
fn parse_hhmmss(s: &str) -> Option<u32> {
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |i: usize| s[i..i + 2].parse::<u32>().ok();
    let (h, m, sec) = (n(0)?, n(2)?, n(4)?);
    (m < 60 && sec < 60).then_some(h * 3600 + m * 60 + sec)
}

/// Split comma-separated AT fields, honouring double quotes (commas inside
/// quotes don't split; quotes are removed).
pub fn fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use Unsolicited as U;

    fn call(index: u8, direction: Direction, state: CallState) -> CallInfo {
        CallInfo {
            index,
            direction,
            state,
            mode: CallMode::Voice,
            multiparty: false,
            number: Some("07700900123".into()),
        }
    }

    /// Every URC line seen in the captures, with its meaning.
    #[test]
    fn captured_lines() {
        let cases = [
            ("*ATREADY: 1", U::Ready),
            ("+CPIN: READY", U::Sim("READY".into())),
            ("SMS DONE", U::SmsReady),
            ("PB DONE", U::PhonebookReady),
            ("+CGEV: EPS PDN ACT 1", U::PacketDomain("EPS PDN ACT 1".into())),
            ("+CGEV: NW ACT 8,10", U::PacketDomain("NW ACT 8,10".into())),
            ("+CGEV: ME DETACH", U::PacketDomain("ME DETACH".into())),
            ("RING", U::Ring),
            ("+CLIP: \"07700900123\",129", U::CallerId { number: Some("07700900123".into()) }),
            ("+COLP: \"07700900123\",129", U::ConnectedLine { number: Some("07700900123".into()) }),
            (
                "+CLCC: 1,0,2,0,0,\"07700900123\",129,\"\"",
                U::Call(call(1, Direction::Outgoing, CallState::Dialling)),
            ),
            (
                "+CLCC: 1,0,3,0,0,\"07700900123\",129,\"\"",
                U::Call(call(1, Direction::Outgoing, CallState::Alerting)),
            ),
            (
                "+CLCC: 1,0,0,0,0,\"07700900123\",129,\"\"",
                U::Call(call(1, Direction::Outgoing, CallState::Active)),
            ),
            (
                "+CLCC: 1,1,4,0,0,\"07700900123\",129,\"\"",
                U::Call(call(1, Direction::Incoming, CallState::Incoming)),
            ),
            (
                "+CLCC: 1,1,6,0,0,\"07700900123\",129,\"\"",
                U::Call(call(1, Direction::Incoming, CallState::Disconnected)),
            ),
            ("VOICE CALL: BEGIN", U::VoiceCallBegin),
            ("VOICE CALL: END: 000035", U::VoiceCallEnd { duration_s: Some(35) }),
            ("VOICE CALL: END: 000122", U::VoiceCallEnd { duration_s: Some(82) }),
            ("VOICE CALL: END", U::VoiceCallEnd { duration_s: None }),
            ("NO CARRIER", U::NoCarrier),
        ];
        for (line, want) in cases {
            assert_eq!(parse(line), want, "{line}");
            assert!(is_urc(line), "{line}");
        }
    }

    #[test]
    fn manual_only_lines() {
        assert_eq!(parse("+CMTI: \"SM\",3"), U::NewSms { storage: "SM".into(), index: 3 });
        assert_eq!(parse("BUSY"), U::Busy);
        assert_eq!(parse("NO ANSWER"), U::NoAnswer);
        assert_eq!(parse("VOICE CALL: END: 012345"), U::VoiceCallEnd { duration_s: Some(3600 + 23 * 60 + 45) });
    }

    #[test]
    fn withheld_numbers_are_none() {
        assert_eq!(parse("+CLIP: \"\",128"), U::CallerId { number: None });
        let U::Call(c) = parse("+CLCC: 2,1,4,0,0,\"\",128") else { panic!() };
        assert_eq!(c.number, None);
        let U::Call(c) = parse("+CLCC: 2,1,4,0,0") else { panic!() };
        assert_eq!(c.number, None);
    }

    #[test]
    fn multiparty_and_other_modes() {
        let U::Call(c) = parse("+CLCC: 3,0,1,9,1,\"+447700900123\",145") else { panic!() };
        assert_eq!((c.index, c.state, c.mode, c.multiparty), (3, CallState::Held, CallMode::Other(9), true));
        assert_eq!(c.number.as_deref(), Some("+447700900123"));
    }

    /// Malformed lines with a known prefix (e.g. a fragment after lost
    /// bytes) are kept as unrecognised text, not half-parsed.
    #[test]
    fn malformed_known_prefixes_are_unrecognised() {
        for line in [
            "+CLCC: 1,0,9,0,0",
            "+CLCC: 1,0",
            "+CLCC: x,0,0,0,0",
            "VOICE CALL: END: 0035",
            "VOICE CALL: END: 006100",
            "+CMTI: \"SM\"",
            "+CLIP: 1,1", // the AT+CLIP? reply, not a caller ID
        ] {
            assert_eq!(parse(line), U::Unrecognised(line.into()), "{line}");
        }
    }

    #[test]
    fn reply_lines_are_not_urcs() {
        for line in ["Manufacturer: INCORPORATED", "+CGMR: A011B15A7682M7", "OK", "ERROR", "NG"] {
            assert!(!is_urc(line), "{line}");
        }
    }

    #[test]
    fn fields_honour_quotes() {
        assert_eq!(fields("1,\"a,b\",2"), ["1", "a,b", "2"]);
        assert_eq!(fields(""), [""]);
        assert_eq!(fields("\"\",128"), ["", "128"]);
    }
}
