//! SMS send / receive over a [`Modem`].
//!
//! All operations live as free functions parameterised by `&mut dyn Modem`,
//! rather than behind a separate `SmsDriver` trait. There's only one real
//! implementation (the modem we already have); tests use [`MockModem`]
//! directly with scripted responses, which keeps the surface area minimal.
//!
//! Mode: text mode (`AT+CMGF=1`) with the GSM character set
//! (`AT+CSCS="GSM"`). PDU mode and UCS2 (which would be needed for full
//! Unicode and reliable multi-line bodies) are deferred.
//!
//! Limitations of this first cut, intentionally accepted to keep the code
//! small and focused:
//!   - **Body capped at 160 characters** to stay within a single SMS
//!     segment. Longer messages return [`SmsError::BodyTooLong`].
//!   - **Single-line bodies only.** A literal newline inside an SMS body
//!     would be split across response lines by the AT parser, breaking
//!     the inbox parser. The send path forbids it; the inbox parser
//!     ignores any trailing lines that aren't preceded by a `+CMGL:`
//!     header.
//!   - **GSM character set only.** Non-ASCII characters (emoji, accented
//!     letters, Cyrillic) are not faithfully transmitted.
//!   - **Number format** is validated only loosely: must start with `+`
//!     or a digit and contain only `+` and digits.
//!
//! All of these are reasonable to revisit later (PDU/UCS2, multipart SMS),
//! but each adds enough parsing/encoding work that it's not worth bundling
//! into the first version.

use crate::modem::{Modem, ModemError};

/// Maximum SMS body length we'll send in a single segment (GSM-7 alphabet).
pub const MAX_BODY_CHARS: usize = 160;

/// Status of a stored SMS, as reported by `+CMGL` / `+CMGR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmsStatus {
    /// Received, not yet read by the user.
    ReceivedUnread,
    /// Received and read.
    ReceivedRead,
    /// Stored locally, not yet sent.
    StoredUnsent,
    /// Stored locally, already sent.
    StoredSent,
    /// Anything we don't recognise.
    Unknown,
}

impl SmsStatus {
    /// Parse the AT-level status string (without surrounding quotes).
    pub fn from_at_string(s: &str) -> Self {
        match s.trim() {
            "REC UNREAD" => SmsStatus::ReceivedUnread,
            "REC READ" => SmsStatus::ReceivedRead,
            "STO UNSENT" => SmsStatus::StoredUnsent,
            "STO SENT" => SmsStatus::StoredSent,
            _ => SmsStatus::Unknown,
        }
    }

    /// Short human-readable label, suitable for shell output.
    pub fn label(&self) -> &'static str {
        match self {
            SmsStatus::ReceivedUnread => "unread",
            SmsStatus::ReceivedRead => "read",
            SmsStatus::StoredUnsent => "draft",
            SmsStatus::StoredSent => "sent",
            SmsStatus::Unknown => "?",
        }
    }
}

/// One stored SMS message, as parsed from `+CMGL` or `+CMGR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmsMessage {
    /// Storage index — used by [`read_message`] and [`delete_message`].
    pub index: u32,
    pub status: SmsStatus,
    /// Sender phone number (or recipient, for stored-sent messages).
    pub sender: String,
    /// Service Centre TimeStamp as the modem reported it (raw string,
    /// usually `"yy/MM/dd,hh:mm:ss±zz"`). Not parsed.
    pub timestamp: String,
    pub body: String,
}

/// Errors returned by the SMS layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmsError {
    /// Underlying modem error.
    Modem(ModemError),
    /// The recipient number was empty or contained disallowed characters.
    InvalidNumber(String),
    /// The body was longer than [`MAX_BODY_CHARS`].
    BodyTooLong { actual: usize, max: usize },
    /// The body contained an embedded newline (unsupported in this version).
    BodyHasNewline,
    /// The requested message index does not exist on the SIM.
    NotFound(u32),
    /// The modem returned data we couldn't parse.
    Parse(String),
}

impl SmsError {
    pub fn display(&self) -> String {
        match self {
            SmsError::Modem(e) => e.display(),
            SmsError::InvalidNumber(n) => format!("invalid number: {}", n),
            SmsError::BodyTooLong { actual, max } => {
                format!("body too long: {} chars (max {})", actual, max)
            }
            SmsError::BodyHasNewline => "body must not contain newlines".to_string(),
            SmsError::NotFound(i) => format!("message {} not found", i),
            SmsError::Parse(s) => format!("parse error: {}", s),
        }
    }
}

impl From<ModemError> for SmsError {
    fn from(e: ModemError) -> Self {
        SmsError::Modem(e)
    }
}

// --- Public operations -------------------------------------------------------

/// Send an SMS to `to` containing `body`. Blocks until the modem confirms
/// the send (or fails).
pub fn send_text(modem: &mut dyn Modem, to: &str, body: &str) -> Result<(), SmsError> {
    validate_number(to)?;
    validate_body(body)?;
    ensure_text_mode(modem)?;
    let cmd = format!("AT+CMGS=\"{}\"", to);
    // Body terminator: Ctrl-Z (0x1A) means "send"; ESC (0x1B) would cancel.
    let mut payload = body.as_bytes().to_vec();
    payload.push(0x1A);
    modem.send_with_body(&cmd, &payload)?;
    Ok(())
}

/// List all messages in the SIM's inbox storage.
pub fn list_inbox(modem: &mut dyn Modem) -> Result<Vec<SmsMessage>, SmsError> {
    ensure_text_mode(modem)?;
    let lines = modem.send_raw("AT+CMGL=\"ALL\"")?;
    Ok(parse_inbox(&lines))
}

/// Read a single message by storage index.
pub fn read_message(modem: &mut dyn Modem, index: u32) -> Result<SmsMessage, SmsError> {
    ensure_text_mode(modem)?;
    let cmd = format!("AT+CMGR={}", index);
    let lines = match modem.send_raw(&cmd) {
        Ok(lines) => lines,
        // The modem returns +CMS ERROR: 321 (invalid memory index) for
        // unknown indices on most SIMCom firmware revisions. Surface that
        // as NotFound for ergonomics.
        Err(ModemError::CmsError(321)) => return Err(SmsError::NotFound(index)),
        Err(e) => return Err(e.into()),
    };
    parse_cmgr(&lines, index).ok_or(SmsError::NotFound(index))
}

/// Delete a message by storage index.
pub fn delete_message(modem: &mut dyn Modem, index: u32) -> Result<(), SmsError> {
    ensure_text_mode(modem)?;
    let cmd = format!("AT+CMGD={}", index);
    match modem.send_raw(&cmd) {
        Ok(_) => Ok(()),
        Err(ModemError::CmsError(321)) => Err(SmsError::NotFound(index)),
        Err(e) => Err(e.into()),
    }
}

// --- Internal helpers --------------------------------------------------------

/// Put the modem into text mode + GSM character set. Idempotent and cheap.
fn ensure_text_mode(modem: &mut dyn Modem) -> Result<(), SmsError> {
    modem.send_raw("AT+CMGF=1")?;
    modem.send_raw("AT+CSCS=\"GSM\"")?;
    Ok(())
}

/// Loose validation: must be non-empty, optionally start with `+`, and
/// otherwise contain only ASCII digits.
fn validate_number(n: &str) -> Result<(), SmsError> {
    let n = n.trim();
    if n.is_empty() {
        return Err(SmsError::InvalidNumber(n.to_string()));
    }
    let body = n.strip_prefix('+').unwrap_or(n);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit()) {
        return Err(SmsError::InvalidNumber(n.to_string()));
    }
    Ok(())
}

fn validate_body(body: &str) -> Result<(), SmsError> {
    if body.contains('\n') || body.contains('\r') {
        return Err(SmsError::BodyHasNewline);
    }
    let len = body.chars().count();
    if len > MAX_BODY_CHARS {
        return Err(SmsError::BodyTooLong {
            actual: len,
            max: MAX_BODY_CHARS,
        });
    }
    Ok(())
}

/// Parse a `+CMGL` response into a list of messages.
///
/// Input is the info-line vector returned by `Modem::send_raw("AT+CMGL=...")`.
/// Headers look like:
/// ```text
/// +CMGL: <index>,"<status>","<sender>",[<alpha>],"<timestamp>"
/// <body line>
/// ```
/// repeated for each message. Bodies are taken as the single line
/// immediately following each header.
///
/// **Empty bodies:** if the next line is itself a `+CMGL:` header (which
/// means this message had an empty body, since the AT layer collapses
/// blank lines), the body is left as `""` and the next header is not
/// consumed. Without this guard, an empty-body message would swallow the
/// next message's header and misreport it as its own body.
pub fn parse_inbox(lines: &[String]) -> Vec<SmsMessage> {
    let mut messages = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(partial) = parse_header(&lines[i], "+CMGL:") {
            i += 1;
            let body = if i < lines.len() && parse_header(&lines[i], "+CMGL:").is_none() {
                let b = lines[i].clone();
                i += 1;
                b
            } else {
                String::new()
            };
            messages.push(SmsMessage {
                index: partial.index,
                status: partial.status,
                sender: partial.sender,
                timestamp: partial.timestamp,
                body,
            });
        } else {
            i += 1;
        }
    }
    messages
}

/// Parse a `+CMGR` response into a single message. Returns `None` if no
/// header is present or it can't be parsed. The caller supplies the index
/// since `+CMGR` does not include one in its response.
pub fn parse_cmgr(lines: &[String], index: u32) -> Option<SmsMessage> {
    for (i, line) in lines.iter().enumerate() {
        if let Some(partial) = parse_header(line, "+CMGR:") {
            let body = lines.get(i + 1).cloned().unwrap_or_default();
            return Some(SmsMessage {
                index,
                status: partial.status,
                sender: partial.sender,
                timestamp: partial.timestamp,
                body,
            });
        }
    }
    None
}

/// The fields a `+CMGL` / `+CMGR` header carries other than the body.
struct Header {
    /// Storage index. `+CMGR` lacks this field; we use 0 in that case and
    /// the caller overwrites with the queried index.
    index: u32,
    status: SmsStatus,
    sender: String,
    timestamp: String,
}

/// Parse the header line of a `+CMGL` or `+CMGR` response.
///
/// `+CMGL: 1,"REC UNREAD","+447...",,"23/04/15,14:30:00+04"`
/// `+CMGR:   "REC READ",  "+447...",,"23/04/15,14:30:00+04"`
fn parse_header(line: &str, prefix: &str) -> Option<Header> {
    let rest = line.trim().strip_prefix(prefix)?;
    let fields = split_csv_fields(rest);

    // CMGL has 5 fields starting with the index; CMGR has 4 starting with
    // the status. Distinguish by whether the first field is numeric.
    let (index, status_idx) = match fields.first().map(|f| f.trim().parse::<u32>()) {
        Some(Ok(idx)) => (idx, 1),
        _ => (0, 0),
    };
    let status = SmsStatus::from_at_string(unquote(fields.get(status_idx)?));
    let sender = unquote(fields.get(status_idx + 1)?).to_string();
    // alpha is fields[status_idx + 2]; we ignore it.
    let timestamp = fields
        .get(status_idx + 3)
        .map(|f| unquote(f).to_string())
        .unwrap_or_default();
    Some(Header {
        index,
        status,
        sender,
        timestamp,
    })
}

/// Split an AT-level comma-separated argument list, respecting double
/// quotes (so a comma inside a quoted string isn't treated as a separator).
/// Quotes are *not* stripped — use [`unquote`] for that.
pub fn split_csv_fields(s: &str) -> Vec<&str> {
    let mut fields = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'"' => in_quotes = !in_quotes,
            b',' if !in_quotes => {
                fields.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    fields.push(&s[start..]);
    fields
}

/// Strip surrounding double quotes (and surrounding whitespace) from an
/// AT field. If the field isn't quoted, returned unchanged (after trim).
pub fn unquote(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::MockModem;

    fn powered_modem() -> MockModem {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m
    }

    // --- validate_number -----------------------------------------------------

    #[test]
    fn valid_numbers() {
        assert!(validate_number("+447123456789").is_ok());
        assert!(validate_number("447123456789").is_ok());
        assert!(validate_number("12345").is_ok());
        assert!(validate_number(" +447123 ").is_ok()); // trimmed
    }

    #[test]
    fn invalid_numbers() {
        assert!(validate_number("").is_err());
        assert!(validate_number("+").is_err());
        assert!(validate_number("abc").is_err());
        assert!(validate_number("07712 345678").is_err()); // embedded space
        assert!(validate_number("+44-7712-345678").is_err()); // dashes
    }

    // --- validate_body -------------------------------------------------------

    #[test]
    fn body_within_limit_ok() {
        assert!(validate_body("hello").is_ok());
        assert!(validate_body(&"a".repeat(MAX_BODY_CHARS)).is_ok());
    }

    #[test]
    fn body_over_limit_errors() {
        let body = "a".repeat(MAX_BODY_CHARS + 1);
        let err = validate_body(&body).unwrap_err();
        assert_eq!(
            err,
            SmsError::BodyTooLong {
                actual: MAX_BODY_CHARS + 1,
                max: MAX_BODY_CHARS,
            }
        );
    }

    #[test]
    fn body_with_newline_errors() {
        assert_eq!(
            validate_body("hello\nworld").unwrap_err(),
            SmsError::BodyHasNewline
        );
        assert_eq!(
            validate_body("hello\rworld").unwrap_err(),
            SmsError::BodyHasNewline
        );
    }

    // --- split_csv_fields / unquote ------------------------------------------

    #[test]
    fn split_simple_fields() {
        assert_eq!(split_csv_fields("a,b,c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn split_respects_quotes() {
        assert_eq!(
            split_csv_fields("1,\"REC READ\",\"+447,123\""),
            vec!["1", "\"REC READ\"", "\"+447,123\""]
        );
    }

    #[test]
    fn split_empty_fields_preserved() {
        assert_eq!(split_csv_fields("a,,b"), vec!["a", "", "b"]);
        assert_eq!(split_csv_fields(",,"), vec!["", "", ""]);
    }

    #[test]
    fn unquote_strips_outer_quotes() {
        assert_eq!(unquote("\"hello\""), "hello");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("\"\""), "");
    }

    #[test]
    fn unquote_handles_whitespace() {
        assert_eq!(unquote("  \"hello\"  "), "hello");
    }

    // --- parse_header / parse_inbox ------------------------------------------

    #[test]
    fn parse_cmgl_header_full() {
        let h =
            parse_header("+CMGL: 1,\"REC UNREAD\",\"+447123\",,\"23/04/15,14:30:00+04\"", "+CMGL:")
                .unwrap();
        assert_eq!(h.index, 1);
        assert_eq!(h.status, SmsStatus::ReceivedUnread);
        assert_eq!(h.sender, "+447123");
        assert_eq!(h.timestamp, "23/04/15,14:30:00+04");
    }

    #[test]
    fn parse_cmgr_header_lacks_index() {
        let h = parse_header(
            "+CMGR: \"REC READ\",\"+447987\",,\"23/04/16,09:15:32+00\"",
            "+CMGR:",
        )
        .unwrap();
        assert_eq!(h.index, 0); // CMGR doesn't carry an index
        assert_eq!(h.status, SmsStatus::ReceivedRead);
        assert_eq!(h.sender, "+447987");
        assert_eq!(h.timestamp, "23/04/16,09:15:32+00");
    }

    #[test]
    fn parse_inbox_two_messages() {
        let lines = vec![
            "+CMGL: 1,\"REC UNREAD\",\"+447123\",,\"23/04/15,14:30:00+04\"".to_string(),
            "Hello there".to_string(),
            "+CMGL: 2,\"REC READ\",\"+447987\",,\"23/04/16,09:15:32+00\"".to_string(),
            "Another message".to_string(),
        ];
        let msgs = parse_inbox(&lines);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].index, 1);
        assert_eq!(msgs[0].body, "Hello there");
        assert_eq!(msgs[0].status, SmsStatus::ReceivedUnread);
        assert_eq!(msgs[1].index, 2);
        assert_eq!(msgs[1].body, "Another message");
        assert_eq!(msgs[1].status, SmsStatus::ReceivedRead);
    }

    #[test]
    fn parse_inbox_empty() {
        assert!(parse_inbox(&[]).is_empty());
    }

    #[test]
    fn parse_inbox_skips_lines_without_header() {
        // A trailing orphan line (e.g. from a previous URC) should not
        // be misinterpreted as a body.
        let lines = vec!["random noise".to_string()];
        assert!(parse_inbox(&lines).is_empty());
    }

    #[test]
    fn parse_inbox_empty_body_does_not_swallow_next_header() {
        // The real bug: message 1 had an empty body (the AT layer
        // collapsed the blank line), so the lines we see are
        // [header1, header2, body2]. Without the guard, header2 would
        // be consumed as message 1's body and message 2 would vanish.
        let lines = vec![
            "+CMGL: 1,\"REC READ\",\"+447123\",,\"24/01/01,12:00:00+00\"".to_string(),
            "+CMGL: 2,\"REC READ\",\"+447987\",,\"24/01/02,09:00:00+00\"".to_string(),
            "real body".to_string(),
        ];
        let msgs = parse_inbox(&lines);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].index, 1);
        assert_eq!(msgs[0].body, "", "first message should have empty body");
        assert_eq!(msgs[1].index, 2);
        assert_eq!(msgs[1].body, "real body");
    }

    #[test]
    fn parse_inbox_trailing_empty_body() {
        // Empty body on the last message — no next line at all.
        let lines = vec![
            "+CMGL: 1,\"REC READ\",\"+447\",,\"24/01/01,12:00:00+00\"".to_string(),
            "first body".to_string(),
            "+CMGL: 2,\"REC READ\",\"+447\",,\"24/01/02,09:00:00+00\"".to_string(),
        ];
        let msgs = parse_inbox(&lines);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].body, "first body");
        assert_eq!(msgs[1].body, "");
    }

    #[test]
    fn parse_inbox_single_empty_body() {
        let lines = vec![
            "+CMGL: 1,\"REC READ\",\"+447\",,\"24/01/01,12:00:00+00\"".to_string(),
        ];
        let msgs = parse_inbox(&lines);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "");
    }

    // --- parse_cmgr ----------------------------------------------------------

    #[test]
    fn parse_cmgr_returns_message_with_caller_index() {
        let lines = vec![
            "+CMGR: \"REC READ\",\"+447987\",,\"23/04/16,09:15:32+00\"".to_string(),
            "the body".to_string(),
        ];
        let msg = parse_cmgr(&lines, 5).unwrap();
        assert_eq!(msg.index, 5);
        assert_eq!(msg.status, SmsStatus::ReceivedRead);
        assert_eq!(msg.sender, "+447987");
        assert_eq!(msg.body, "the body");
    }

    #[test]
    fn parse_cmgr_returns_none_when_no_header() {
        assert!(parse_cmgr(&[], 1).is_none());
        assert!(parse_cmgr(&["unrelated".to_string()], 1).is_none());
    }

    // --- end-to-end against MockModem ----------------------------------------

    #[test]
    fn send_text_runs_correct_command_sequence_and_appends_ctrl_z() {
        let mut m = powered_modem();
        m.on_with_body("AT+CMGS=\"+447123\"", Ok(vec!["+CMGS: 42".into()]));
        send_text(&mut m, "+447123", "hello").unwrap();

        assert_eq!(m.raw_log, vec!["AT+CMGF=1", "AT+CSCS=\"GSM\""]);
        assert_eq!(m.body_log.len(), 1);
        assert_eq!(m.body_log[0].0, "AT+CMGS=\"+447123\"");
        assert_eq!(m.body_log[0].1, b"hello\x1a");
    }

    #[test]
    fn send_text_propagates_modem_error() {
        let mut m = powered_modem();
        m.on_with_body("AT+CMGS=\"+447123\"", Err(ModemError::CmsError(310)));
        let err = send_text(&mut m, "+447123", "hi").unwrap_err();
        assert_eq!(err, SmsError::Modem(ModemError::CmsError(310)));
    }

    #[test]
    fn send_text_rejects_invalid_number_without_calling_modem() {
        let mut m = powered_modem();
        let err = send_text(&mut m, "abc", "hi").unwrap_err();
        assert!(matches!(err, SmsError::InvalidNumber(_)));
        assert!(m.raw_log.is_empty());
        assert!(m.body_log.is_empty());
    }

    #[test]
    fn send_text_rejects_long_body_without_calling_modem() {
        let mut m = powered_modem();
        let body = "a".repeat(MAX_BODY_CHARS + 1);
        let err = send_text(&mut m, "+447", &body).unwrap_err();
        assert!(matches!(err, SmsError::BodyTooLong { .. }));
        assert!(m.body_log.is_empty());
    }

    #[test]
    fn list_inbox_parses_modem_response() {
        let mut m = powered_modem();
        m.on_raw(
            "AT+CMGL=\"ALL\"",
            Ok(vec![
                "+CMGL: 1,\"REC UNREAD\",\"+447\",,\"23/04/15,14:30:00+04\"".to_string(),
                "first".to_string(),
                "+CMGL: 2,\"REC READ\",\"+447\",,\"23/04/16,09:15:32+00\"".to_string(),
                "second".to_string(),
            ]),
        );
        let msgs = list_inbox(&mut m).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].body, "first");
        assert_eq!(msgs[1].body, "second");
    }

    #[test]
    fn read_message_returns_message_at_index() {
        let mut m = powered_modem();
        m.on_raw(
            "AT+CMGR=3",
            Ok(vec![
                "+CMGR: \"REC UNREAD\",\"+447\",,\"23/04/15,14:30:00+04\"".to_string(),
                "the body".to_string(),
            ]),
        );
        let msg = read_message(&mut m, 3).unwrap();
        assert_eq!(msg.index, 3);
        assert_eq!(msg.body, "the body");
    }

    #[test]
    fn read_message_maps_invalid_index_error() {
        let mut m = powered_modem();
        m.on_raw("AT+CMGR=99", Err(ModemError::CmsError(321)));
        assert_eq!(
            read_message(&mut m, 99).unwrap_err(),
            SmsError::NotFound(99)
        );
    }

    #[test]
    fn delete_message_calls_correct_command() {
        let mut m = powered_modem();
        delete_message(&mut m, 7).unwrap();
        assert_eq!(m.raw_log.last().unwrap(), "AT+CMGD=7");
    }

    #[test]
    fn delete_message_maps_invalid_index_error() {
        let mut m = powered_modem();
        m.on_raw("AT+CMGD=99", Err(ModemError::CmsError(321)));
        assert_eq!(
            delete_message(&mut m, 99).unwrap_err(),
            SmsError::NotFound(99)
        );
    }

    // --- SmsStatus -----------------------------------------------------------

    #[test]
    fn status_round_trip() {
        for s in [
            ("REC UNREAD", SmsStatus::ReceivedUnread),
            ("REC READ", SmsStatus::ReceivedRead),
            ("STO UNSENT", SmsStatus::StoredUnsent),
            ("STO SENT", SmsStatus::StoredSent),
            ("anything else", SmsStatus::Unknown),
        ] {
            assert_eq!(SmsStatus::from_at_string(s.0), s.1);
        }
    }
}
