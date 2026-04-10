//! AT command response parser.
//!
//! The parser consumes raw bytes from a modem UART and emits a sequence
//! of [`AtEvent`] values: complete text lines and SMS-body prompts. It is
//! deliberately oblivious to command semantics — classifying a line as a
//! final result, a URC, or an info response is the job of the layer above
//! (see [`classify`]).
//!
//! Parser invariants:
//!   - Bytes are fed in arbitrary chunks; the parser buffers partial lines.
//!   - `\r` and `\n` both act as line terminators; any run of them between
//!     non-terminator bytes finalises at most one line (empty lines are
//!     discarded — they are padding in the AT protocol).
//!   - The two-byte sequence `"> "` at the start of a pending line is
//!     emitted as [`AtEvent::Prompt`]. It is the only event that is not
//!     line-terminated; on the A7682E it only appears after `AT+CMGS`
//!     (and similar) as the modem's request for a binary payload.
//!   - [`AtParser::reset`] clears any partial line; useful before sending
//!     a fresh command so stale bytes from a prior timeout are discarded.

/// An event produced by the AT parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtEvent {
    /// A complete line of text received from the modem.
    Line(String),
    /// The modem sent `"> "` indicating it is ready to receive a binary
    /// payload (e.g. an SMS body terminated by Ctrl-Z).
    Prompt,
}

/// Incremental parser that splits a byte stream from the modem into
/// [`AtEvent`] values.
pub struct AtParser {
    buf: Vec<u8>,
}

impl AtParser {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Feed bytes into the parser and return any events that were completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<AtEvent> {
        let mut events = Vec::new();
        for &b in bytes {
            match b {
                b'\r' | b'\n' => {
                    if !self.buf.is_empty() {
                        let line = String::from_utf8_lossy(&self.buf).into_owned();
                        self.buf.clear();
                        events.push(AtEvent::Line(line));
                    }
                    // Empty lines are silently discarded.
                }
                _ => {
                    self.buf.push(b);
                    // "> " at the start of a pending line is the SMS body
                    // prompt. The modem never sends a legitimate info line
                    // beginning with "> ", so this is unambiguous.
                    if self.buf.as_slice() == b"> " {
                        self.buf.clear();
                        events.push(AtEvent::Prompt);
                    }
                }
            }
        }
        events
    }

    /// Drop any partially-received line. Call before issuing a new command
    /// if a previous one timed out, to avoid mixing stale bytes into the
    /// new response.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

impl Default for AtParser {
    fn default() -> Self {
        Self::new()
    }
}

/// How a received line should be treated by the layer above the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineClass {
    /// Final success result code (`OK`).
    Ok,
    /// Final generic error result code (`ERROR`).
    Error,
    /// Final `+CME ERROR: <code>` — equipment/SIM-level error.
    CmeError(i32),
    /// Final `+CMS ERROR: <code>` — messaging-layer error.
    CmsError(i32),
    /// Final `NO CARRIER` — the cellular link dropped.
    NoCarrier,
    /// An unsolicited result code the parser recognises.
    Urc(Urc),
    /// A line that echoes the command we just sent (e.g. `"AT+CSQ"`).
    Echo,
    /// An ordinary information response — part of the current command's
    /// output, to be collected by the caller until a final result arrives.
    Info,
}

/// Recognised unsolicited result codes.
///
/// Deliberately minimal — URCs that overlap with solicited responses
/// (e.g. `+CREG:`, `+CPIN:`) are left as [`LineClass::Info`] so they can
/// be returned as command output. Only entries that are *always* URCs
/// appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Urc {
    /// New SMS stored at the given index in the given storage area.
    NewSms { storage: String, index: u32 },
    /// Incoming call ringing.
    Ring,
}

/// Classify a single line received from the modem.
pub fn classify(line: &str) -> LineClass {
    let trimmed = line.trim();
    match trimmed {
        "OK" => return LineClass::Ok,
        "ERROR" => return LineClass::Error,
        "NO CARRIER" => return LineClass::NoCarrier,
        "RING" => return LineClass::Urc(Urc::Ring),
        _ => {}
    }
    if let Some(rest) = trimmed.strip_prefix("+CME ERROR:") {
        return LineClass::CmeError(rest.trim().parse().unwrap_or(-1));
    }
    if let Some(rest) = trimmed.strip_prefix("+CMS ERROR:") {
        return LineClass::CmsError(rest.trim().parse().unwrap_or(-1));
    }
    if let Some(rest) = trimmed.strip_prefix("+CMTI:") {
        if let Some((storage, index)) = parse_cmti(rest) {
            return LineClass::Urc(Urc::NewSms { storage, index });
        }
    }
    // Command echo: the modem repeats the command we sent before responding.
    // Any line starting with "AT" (case-sensitive — the modem preserves case)
    // is almost certainly an echo since legitimate info responses begin with
    // '+' or are result codes handled above.
    if trimmed.starts_with("AT") {
        return LineClass::Echo;
    }
    LineClass::Info
}

/// Parse the argument portion of `+CMTI: "SM",5` into `("SM", 5)`.
fn parse_cmti(rest: &str) -> Option<(String, u32)> {
    let rest = rest.trim();
    let (storage_q, idx_s) = rest.split_once(',')?;
    let storage = storage_q.trim().trim_matches('"').to_string();
    let index: u32 = idx_s.trim().parse().ok()?;
    Some((storage, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- AtParser ------------------------------------------------------------

    #[test]
    fn parses_single_line() {
        let mut p = AtParser::new();
        let events = p.feed(b"OK\r\n");
        assert_eq!(events, vec![AtEvent::Line("OK".into())]);
    }

    #[test]
    fn parses_crlf_leading_empty_line() {
        // Modem responses often start with a blank CRLF before the actual content.
        let mut p = AtParser::new();
        let events = p.feed(b"\r\n+CSQ: 15,99\r\n\r\nOK\r\n");
        assert_eq!(
            events,
            vec![
                AtEvent::Line("+CSQ: 15,99".into()),
                AtEvent::Line("OK".into()),
            ]
        );
    }

    #[test]
    fn handles_chunked_feed_across_line_boundary() {
        let mut p = AtParser::new();
        let mut events = Vec::new();
        events.extend(p.feed(b"+CR"));
        events.extend(p.feed(b"EG: 0,1"));
        events.extend(p.feed(b"\r\n"));
        assert_eq!(events, vec![AtEvent::Line("+CREG: 0,1".into())]);
    }

    #[test]
    fn handles_chunked_feed_single_byte() {
        let mut p = AtParser::new();
        let mut events = Vec::new();
        for b in b"OK\r\n" {
            events.extend(p.feed(&[*b]));
        }
        assert_eq!(events, vec![AtEvent::Line("OK".into())]);
    }

    #[test]
    fn emits_prompt_after_crlf() {
        let mut p = AtParser::new();
        let events = p.feed(b"\r\n> ");
        assert_eq!(events, vec![AtEvent::Prompt]);
    }

    #[test]
    fn prompt_can_arrive_in_chunks() {
        let mut p = AtParser::new();
        let mut events = Vec::new();
        events.extend(p.feed(b"\r\n>"));
        events.extend(p.feed(b" "));
        assert_eq!(events, vec![AtEvent::Prompt]);
    }

    #[test]
    fn discards_empty_lines() {
        let mut p = AtParser::new();
        let events = p.feed(b"\r\n\r\n\r\n");
        assert!(events.is_empty());
    }

    #[test]
    fn bare_lf_terminates_line() {
        let mut p = AtParser::new();
        let events = p.feed(b"OK\n");
        assert_eq!(events, vec![AtEvent::Line("OK".into())]);
    }

    #[test]
    fn reset_discards_partial_line() {
        let mut p = AtParser::new();
        p.feed(b"partial");
        p.reset();
        let events = p.feed(b"OK\r\n");
        assert_eq!(events, vec![AtEvent::Line("OK".into())]);
    }

    #[test]
    fn csq_and_ok_in_one_feed() {
        let mut p = AtParser::new();
        let events = p.feed(b"AT+CSQ\r\r\n+CSQ: 20,99\r\n\r\nOK\r\n");
        assert_eq!(
            events,
            vec![
                AtEvent::Line("AT+CSQ".into()),
                AtEvent::Line("+CSQ: 20,99".into()),
                AtEvent::Line("OK".into()),
            ]
        );
    }

    // --- classify ------------------------------------------------------------

    #[test]
    fn classify_ok() {
        assert_eq!(classify("OK"), LineClass::Ok);
    }

    #[test]
    fn classify_error() {
        assert_eq!(classify("ERROR"), LineClass::Error);
    }

    #[test]
    fn classify_cme_error() {
        assert_eq!(classify("+CME ERROR: 13"), LineClass::CmeError(13));
    }

    #[test]
    fn classify_cms_error() {
        assert_eq!(classify("+CMS ERROR: 310"), LineClass::CmsError(310));
    }

    #[test]
    fn classify_no_carrier() {
        assert_eq!(classify("NO CARRIER"), LineClass::NoCarrier);
    }

    #[test]
    fn classify_ring_urc() {
        assert_eq!(classify("RING"), LineClass::Urc(Urc::Ring));
    }

    #[test]
    fn classify_cmti_urc() {
        assert_eq!(
            classify("+CMTI: \"SM\",5"),
            LineClass::Urc(Urc::NewSms {
                storage: "SM".into(),
                index: 5,
            })
        );
    }

    #[test]
    fn classify_echo() {
        assert_eq!(classify("AT+CSQ"), LineClass::Echo);
        assert_eq!(classify("ATE0"), LineClass::Echo);
    }

    #[test]
    fn classify_info_line() {
        assert_eq!(classify("+CSQ: 15,99"), LineClass::Info);
        assert_eq!(classify("+CREG: 0,1"), LineClass::Info);
        assert_eq!(classify("+CPIN: READY"), LineClass::Info);
    }

    #[test]
    fn classify_tolerates_surrounding_whitespace() {
        assert_eq!(classify("  OK  "), LineClass::Ok);
        assert_eq!(classify(" +CME ERROR: 7 "), LineClass::CmeError(7));
    }

    #[test]
    fn classify_malformed_cmti_falls_back_to_info() {
        // Missing comma: not a valid URC payload, treat as info.
        assert_eq!(classify("+CMTI: garbage"), LineClass::Info);
    }
}
