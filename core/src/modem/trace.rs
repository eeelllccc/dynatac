//! Text format for recorded modem UART traffic ("traces").
//!
//! A trace is the exact byte stream exchanged with the modem, with a
//! timestamp per chunk. Traces are captured on hardware
//! (`device/examples/modem_capture.rs`) and replayed in tests so the modem
//! simulator can be checked against real behaviour.
//!
//! One record per line:
//!
//! ```text
//! TRACE <t_ms> <dir> <payload>
//! ```
//!
//! - `t_ms`: milliseconds since capture start (decimal `u64`).
//! - `dir`: `<` received from the modem, `>` sent to the modem,
//!   `#` a note from the capture tool (marker, power event, RI edge).
//! - `payload`: the bytes, escaped. Printable ASCII other than `\` is
//!   literal; `\\`, `\r`, `\n` and `\xHH` cover everything else. The
//!   payload may be empty and may contain spaces.
//!
//! Lines not starting with `TRACE ` are not records and are skipped, so a
//! raw serial-monitor log (with ESP-IDF log lines mixed in) parses as-is.
//!
//! Invariant: `parse_line(&r.format()) == Some(Ok(r))` for every record.
//! A received chunk's boundaries reflect when the host read the UART, not
//! how the modem framed its output.

use std::fmt;

use super::at::{AtEvent, AtParser};

const PREFIX: &str = "TRACE ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Modem → host.
    Rx,
    /// Host → modem.
    Tx,
    /// Annotation from the capture tool; not UART traffic.
    Note,
}

impl Direction {
    fn symbol(self) -> char {
        match self {
            Direction::Rx => '<',
            Direction::Tx => '>',
            Direction::Note => '#',
        }
    }

    fn from_symbol(s: &str) -> Option<Self> {
        match s {
            "<" => Some(Direction::Rx),
            ">" => Some(Direction::Tx),
            "#" => Some(Direction::Note),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRecord {
    pub t_ms: u64,
    pub dir: Direction,
    pub bytes: Vec<u8>,
}

impl TraceRecord {
    pub fn new(t_ms: u64, dir: Direction, bytes: &[u8]) -> Self {
        Self { t_ms, dir, bytes: bytes.to_vec() }
    }

    /// Render as one trace line (no trailing newline).
    pub fn format(&self) -> String {
        format!("{}{} {} {}", PREFIX, self.t_ms, self.dir.symbol(), escape(&self.bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceError {
    BadTimestamp,
    BadDirection,
    /// A `\` escape that isn't `\\`, `\r`, `\n` or `\xHH`.
    BadEscape,
    /// A byte outside printable ASCII appeared unescaped.
    UnescapedByte,
}

impl fmt::Display for TraceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            TraceError::BadTimestamp => "bad timestamp",
            TraceError::BadDirection => "bad direction",
            TraceError::BadEscape => "bad escape",
            TraceError::UnescapedByte => "unescaped non-printable byte",
        };
        f.write_str(s)
    }
}

/// Parse one line. `None` if it isn't a trace record (no `TRACE ` prefix);
/// `Some(Err)` if it claims to be one but is malformed.
pub fn parse_line(line: &str) -> Option<Result<TraceRecord, TraceError>> {
    let rest = line.trim_end_matches(['\r', '\n']).strip_prefix(PREFIX)?;
    Some(parse_fields(rest))
}

fn parse_fields(rest: &str) -> Result<TraceRecord, TraceError> {
    let (t, rest) = rest.split_once(' ').ok_or(TraceError::BadTimestamp)?;
    let t_ms = t.parse().map_err(|_| TraceError::BadTimestamp)?;
    // The payload may be empty, in which case the separating space after
    // the direction is still present but nothing follows it.
    let (dir, payload) = rest.split_once(' ').ok_or(TraceError::BadDirection)?;
    let dir = Direction::from_symbol(dir).ok_or(TraceError::BadDirection)?;
    Ok(TraceRecord { t_ms, dir, bytes: unescape(payload)? })
}

/// Parse every record in a text, skipping non-record lines. Records are
/// returned in timestamp order (stable, so equal timestamps keep their
/// order in the text). Captures are written by two threads, so lines in
/// the file can be slightly out of time order.
pub fn parse_trace(text: &str) -> Result<Vec<TraceRecord>, (usize, TraceError)> {
    let mut records = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(r) = parse_line(line) {
            records.push(r.map_err(|e| (i + 1, e))?);
        }
    }
    records.sort_by_key(|r| r.t_ms);
    Ok(records)
}

/// One protocol-level event in a trace: what a reader of the AT dialogue
/// cares about, with chunk boundaries gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// A command we sent, without its trailing `\r`.
    Sent(String),
    /// A complete line from the modem (empty lines dropped, as `AtParser`).
    Line(String),
    /// The modem's `> ` prompt.
    Prompt,
    /// A capture-tool note (marker, power event, RI edge).
    Note(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedStep {
    pub t_ms: u64,
    pub step: Step,
}

/// The trace as a sequence of protocol steps. Received bytes go through
/// one `AtParser` in order, so lines split across chunks are rejoined;
/// a line is timed by the chunk that completed it.
///
/// Requires `records` in time order (as `parse_trace` returns them).
pub fn timeline(records: &[TraceRecord]) -> Vec<TimedStep> {
    let mut parser = AtParser::new();
    let mut steps = Vec::new();
    for r in records {
        let at = |step| TimedStep { t_ms: r.t_ms, step };
        match r.dir {
            Direction::Rx => steps.extend(parser.feed(&r.bytes).into_iter().map(|e| {
                at(match e {
                    AtEvent::Line(l) => Step::Line(l),
                    AtEvent::Prompt => Step::Prompt,
                })
            })),
            Direction::Tx => {
                let text = String::from_utf8_lossy(&r.bytes);
                steps.push(at(Step::Sent(text.trim_end_matches('\r').to_string())));
            }
            Direction::Note => {
                steps.push(at(Step::Note(String::from_utf8_lossy(&r.bytes).into_owned())))
            }
        }
    }
    steps
}

pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{:02X}", b)),
        }
    }
    out
}

pub fn unescape(s: &str) -> Result<Vec<u8>, TraceError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            if !(0x20..=0x7e).contains(&b) {
                return Err(TraceError::UnescapedByte);
            }
            out.push(b);
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            Some(b'\\') => out.push(b'\\'),
            Some(b'r') => out.push(b'\r'),
            Some(b'n') => out.push(b'\n'),
            Some(b'x') => {
                let hex = s.get(i + 2..i + 4).ok_or(TraceError::BadEscape)?;
                if !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Err(TraceError::BadEscape);
                }
                out.push(u8::from_str_radix(hex, 16).map_err(|_| TraceError::BadEscape)?);
                i += 2;
            }
            _ => return Err(TraceError::BadEscape),
        }
        i += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_leaves_printable_ascii() {
        assert_eq!(escape(b"AT+CLCC=1"), "AT+CLCC=1");
        assert_eq!(escape(b"+CLIP: \"07700\",129"), "+CLIP: \"07700\",129");
    }

    #[test]
    fn escape_line_endings_and_backslash() {
        assert_eq!(escape(b"OK\r\n"), "OK\\r\\n");
        assert_eq!(escape(b"a\\b"), "a\\\\b");
    }

    #[test]
    fn escape_non_printable_as_hex() {
        assert_eq!(escape(&[0x00, 0x1A, 0x7F, 0xFF]), "\\x00\\x1A\\x7F\\xFF");
    }

    #[test]
    fn every_byte_round_trips() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(unescape(&escape(&all)).unwrap(), all);
    }

    #[test]
    fn unescape_accepts_lowercase_hex() {
        assert_eq!(unescape("\\x1a").unwrap(), vec![0x1A]);
    }

    #[test]
    fn unescape_rejects_bad_escapes() {
        assert_eq!(unescape("\\q"), Err(TraceError::BadEscape));
        assert_eq!(unescape("\\"), Err(TraceError::BadEscape));
        assert_eq!(unescape("\\x1"), Err(TraceError::BadEscape));
        assert_eq!(unescape("\\xZZ"), Err(TraceError::BadEscape));
    }

    #[test]
    fn unescape_rejects_raw_control_bytes() {
        assert_eq!(unescape("OK\r"), Err(TraceError::UnescapedByte));
    }

    #[test]
    fn record_round_trips() {
        let records = [
            TraceRecord::new(0, Direction::Tx, b"ATD07700900123;\r"),
            TraceRecord::new(1234, Direction::Rx, b"\r\nOK\r\n\r\nVOICE CALL: BEGIN\r\n"),
            TraceRecord::new(u64::MAX, Direction::Note, b"mark: remote hang-up"),
            TraceRecord::new(5, Direction::Rx, b""),
            TraceRecord::new(6, Direction::Rx, b" leading and trailing "),
        ];
        for r in records {
            assert_eq!(parse_line(&r.format()), Some(Ok(r.clone())), "{}", r.format());
        }
    }

    #[test]
    fn format_is_human_readable() {
        let r = TraceRecord::new(42, Direction::Rx, b"\r\nOK\r\n");
        assert_eq!(r.format(), "TRACE 42 < \\r\\nOK\\r\\n");
    }

    #[test]
    fn non_record_lines_are_skipped() {
        assert_eq!(parse_line("I (1234) dynatac: booting"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("TRACE"), None);
    }

    #[test]
    fn malformed_records_are_errors() {
        assert_eq!(parse_line("TRACE x < OK"), Some(Err(TraceError::BadTimestamp)));
        assert_eq!(parse_line("TRACE 12"), Some(Err(TraceError::BadTimestamp)));
        assert_eq!(parse_line("TRACE 12 ? OK"), Some(Err(TraceError::BadDirection)));
        assert_eq!(parse_line("TRACE 12 <"), Some(Err(TraceError::BadDirection)));
    }

    #[test]
    fn tolerates_crlf_from_serial_monitor() {
        let r = parse_line("TRACE 7 < OK\\r\\n\r\n").unwrap().unwrap();
        assert_eq!(r.bytes, b"OK\r\n");
    }

    #[test]
    fn parse_trace_skips_noise_and_sorts_by_time() {
        let text = "\
I (100) boot: hello
TRACE 20 < OK\\r\\n
TRACE 10 > AT\\r
W (200) something
TRACE 20 # after-ok
";
        let records = parse_trace(text).unwrap();
        assert_eq!(
            records,
            vec![
                TraceRecord::new(10, Direction::Tx, b"AT\r"),
                TraceRecord::new(20, Direction::Rx, b"OK\r\n"),
                TraceRecord::new(20, Direction::Note, b"after-ok"),
            ]
        );
    }

    #[test]
    fn timeline_rejoins_lines_split_across_chunks() {
        // Real split seen in every `ATI` reply: chunk ends mid line-ending.
        let records = [
            TraceRecord::new(1, Direction::Tx, b"ATI\r"),
            TraceRecord::new(2, Direction::Rx, b"\r\nModel: A7682E\r\n+GCAP: +DS\r"),
            TraceRecord::new(3, Direction::Rx, b"\n\r\nOK\r\n"),
            TraceRecord::new(4, Direction::Note, b"mark: done"),
        ];
        let steps: Vec<_> = timeline(&records).into_iter().map(|s| (s.t_ms, s.step)).collect();
        assert_eq!(
            steps,
            vec![
                (1, Step::Sent("ATI".into())),
                (2, Step::Line("Model: A7682E".into())),
                (2, Step::Line("+GCAP: +DS".into())),
                (3, Step::Line("OK".into())),
                (4, Step::Note("mark: done".into())),
            ]
        );
    }

    #[test]
    fn timeline_times_a_line_by_the_chunk_that_completes_it() {
        let records = [
            TraceRecord::new(10, Direction::Rx, b"\r\nVOICE CA"),
            TraceRecord::new(20, Direction::Rx, b"LL: BEGIN\r\n"),
        ];
        assert_eq!(
            timeline(&records),
            vec![TimedStep { t_ms: 20, step: Step::Line("VOICE CALL: BEGIN".into()) }]
        );
    }

    #[test]
    fn timeline_emits_prompt() {
        let records = [TraceRecord::new(5, Direction::Rx, b"\r\n> ")];
        assert_eq!(timeline(&records), vec![TimedStep { t_ms: 5, step: Step::Prompt }]);
    }

    #[test]
    fn parse_trace_reports_line_number_of_bad_record() {
        let text = "noise\nTRACE 1 < ok\nTRACE 2 < bad\\q\n";
        assert_eq!(parse_trace(text), Err((3, TraceError::BadEscape)));
    }
}
