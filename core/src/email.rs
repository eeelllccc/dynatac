//! Email message serialization and SMTP stream abstraction.
//!
//! This module is the pure-logic side of email sending. It defines:
//!   - [`Email`] — a plain-text email message that serializes to an
//!     RFC 5322 / RFC 5321 wire-format payload suitable for the SMTP
//!     `DATA` command
//!   - [`SmtpStream`] — a minimal byte-oriented stream the device
//!     implements with TLS, and that tests substitute with
//!     [`MockSmtpStream`]
//!   - [`base64_encode`] — used by AUTH LOGIN
//!
//! The actual SMTP protocol state machine lives in [`SmtpSession`]
//! (added in the next step).
//!
//! Caller invariants:
//!   - `Email::to_rfc5322` produces the message body only — no leading
//!     SMTP commands and no trailing `.\r\n` terminator. The session
//!     wraps the body with `DATA` / terminator.
//!   - `SmtpStream::read_line` returns one CRLF-terminated server line
//!     with the CRLF stripped. Multi-line replies are read with
//!     repeated calls.

use std::collections::VecDeque;

/// A plain-text email message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
}

impl Email {
    /// Serialize to an RFC 5322 message suitable for SMTP `DATA`.
    ///
    /// - All lines are CRLF-terminated.
    /// - The body is split on `\n`; any pre-existing trailing `\r` on a
    ///   segment is stripped so we don't emit `\r\r\n`.
    /// - Body lines starting with `.` are dot-stuffed per RFC 5321 §4.5.2.
    /// - The `\r\n.\r\n` end-of-data marker is **not** included; the
    ///   SMTP session appends it.
    pub fn to_rfc5322(&self) -> String {
        let mut out = String::new();
        out.push_str("From: ");
        out.push_str(&self.from);
        out.push_str("\r\n");
        out.push_str("To: ");
        out.push_str(&self.to);
        out.push_str("\r\n");
        out.push_str("Subject: ");
        out.push_str(&self.subject);
        out.push_str("\r\n");
        out.push_str("MIME-Version: 1.0\r\n");
        out.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
        out.push_str("\r\n"); // header / body separator

        for segment in self.body.split('\n') {
            let segment = segment.strip_suffix('\r').unwrap_or(segment);
            if segment.starts_with('.') {
                out.push('.'); // dot-stuffing
            }
            out.push_str(segment);
            out.push_str("\r\n");
        }
        out
    }
}

// --- SmtpStream trait + mock ------------------------------------------------

/// Byte-level stream the SMTP session reads from and writes to.
///
/// The device implementation wraps a TLS socket; tests use
/// [`MockSmtpStream`] with scripted server responses.
pub trait SmtpStream {
    /// Read one CRLF-terminated server line. Returned string excludes the CRLF.
    fn read_line(&mut self) -> Result<String, String>;
    /// Write all bytes to the stream. Implementations may buffer.
    fn write_all(&mut self, data: &[u8]) -> Result<(), String>;
}

/// Test double: returns scripted server lines in order and records all
/// bytes the client writes.
pub struct MockSmtpStream {
    server_lines: VecDeque<String>,
    pub written: Vec<u8>,
}

impl MockSmtpStream {
    pub fn new() -> Self {
        Self {
            server_lines: VecDeque::new(),
            written: Vec::new(),
        }
    }

    /// Queue a line the server will return on the next `read_line` call.
    pub fn push_line(&mut self, line: &str) -> &mut Self {
        self.server_lines.push_back(line.to_string());
        self
    }

    /// Everything the client has written so far, as a UTF-8 string.
    pub fn written_str(&self) -> String {
        String::from_utf8_lossy(&self.written).into_owned()
    }

    /// True if every queued server line has been consumed.
    pub fn is_drained(&self) -> bool {
        self.server_lines.is_empty()
    }
}

impl Default for MockSmtpStream {
    fn default() -> Self {
        Self::new()
    }
}

impl SmtpStream for MockSmtpStream {
    fn read_line(&mut self) -> Result<String, String> {
        self.server_lines
            .pop_front()
            .ok_or_else(|| "mock: no more server lines".to_string())
    }
    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.written.extend_from_slice(data);
        Ok(())
    }
}

// --- SmtpStreamFactory ------------------------------------------------------

/// Opens SMTP streams on demand.
///
/// Production code returns a fresh TLS-wrapped TCP connection per call;
/// the mock returns a borrow of an internal scripted [`MockSmtpStream`]
/// the test can inspect afterward.
///
/// Each `open` invalidates any previously-returned stream — callers must
/// finish one session before opening another.
pub trait SmtpStreamFactory {
    fn open(&mut self, host: &str, port: u16) -> Result<&mut dyn SmtpStream, String>;
}

/// Test double for [`SmtpStreamFactory`]. Owns a single [`MockSmtpStream`]
/// the test pre-loads with server responses and inspects after the program
/// has run.
pub struct MockSmtpStreamFactory {
    pub stream: MockSmtpStream,
    pub open_count: usize,
    /// If set, `open` returns this error instead of a stream. Used to test
    /// the "couldn't connect" path.
    pub fail_with: Option<String>,
}

impl MockSmtpStreamFactory {
    pub fn new() -> Self {
        Self {
            stream: MockSmtpStream::new(),
            open_count: 0,
            fail_with: None,
        }
    }
}

impl Default for MockSmtpStreamFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SmtpStreamFactory for MockSmtpStreamFactory {
    fn open(&mut self, _host: &str, _port: u16) -> Result<&mut dyn SmtpStream, String> {
        self.open_count += 1;
        if let Some(e) = &self.fail_with {
            return Err(e.clone());
        }
        Ok(&mut self.stream)
    }
}

// --- base64 -----------------------------------------------------------------

const BASE64_CHARSET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 base64 encode (no line wrapping). Used for SMTP AUTH LOGIN.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks_exact(3);
    for c in chunks.by_ref() {
        let n = ((c[0] as u32) << 16) | ((c[1] as u32) << 8) | (c[2] as u32);
        out.push(BASE64_CHARSET[((n >> 18) & 0x3f) as usize] as char);
        out.push(BASE64_CHARSET[((n >> 12) & 0x3f) as usize] as char);
        out.push(BASE64_CHARSET[((n >> 6) & 0x3f) as usize] as char);
        out.push(BASE64_CHARSET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(BASE64_CHARSET[((n >> 18) & 0x3f) as usize] as char);
            out.push(BASE64_CHARSET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(BASE64_CHARSET[((n >> 18) & 0x3f) as usize] as char);
            out.push(BASE64_CHARSET[((n >> 12) & 0x3f) as usize] as char);
            out.push(BASE64_CHARSET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

// --- SMTP session state machine --------------------------------------------

/// Drives the SMTP submission protocol over an [`SmtpStream`].
///
/// Implements the smallest viable session for Gmail submission on port 465
/// (implicit TLS): greeting → EHLO → AUTH LOGIN → MAIL FROM → RCPT TO →
/// DATA → QUIT. STARTTLS is not used; the stream is already encrypted by
/// the time we see it.
///
/// Caller invariants:
///   - `stream` must already be connected and TLS-wrapped.
///   - `username` is the full Gmail address; `password` is a 16-character
///     Google App Password (regular passwords are rejected by Google).
pub struct SmtpSession;

impl SmtpSession {
    /// Send an email. Returns `Ok(())` on success or `Err(step)` describing
    /// which step failed and why.
    pub fn send(
        stream: &mut dyn SmtpStream,
        hostname: &str,
        username: &str,
        password: &str,
        email: &Email,
    ) -> Result<(), String> {
        // 1. Greeting
        let (code, _) = read_reply(stream)?;
        if code != 220 {
            return Err(format!("greeting: expected 220, got {}", code));
        }

        // 2. EHLO
        write_line(stream, &format!("EHLO {}", hostname))?;
        let (code, _) = read_reply(stream)?;
        if code != 250 {
            return Err(format!("EHLO: expected 250, got {}", code));
        }

        // 3. AUTH LOGIN
        write_line(stream, "AUTH LOGIN")?;
        let (code, _) = read_reply(stream)?;
        if code != 334 {
            return Err(format!("AUTH LOGIN: expected 334, got {}", code));
        }

        // 3a. username (base64)
        write_line(stream, &base64_encode(username.as_bytes()))?;
        let (code, _) = read_reply(stream)?;
        if code != 334 {
            return Err(format!("AUTH username: expected 334, got {}", code));
        }

        // 3b. password (base64)
        write_line(stream, &base64_encode(password.as_bytes()))?;
        let (code, _) = read_reply(stream)?;
        if code != 235 {
            return Err(format!("AUTH password: expected 235, got {}", code));
        }

        // 4. MAIL FROM
        write_line(stream, &format!("MAIL FROM:<{}>", email.from))?;
        let (code, _) = read_reply(stream)?;
        if code != 250 {
            return Err(format!("MAIL FROM: expected 250, got {}", code));
        }

        // 5. RCPT TO
        write_line(stream, &format!("RCPT TO:<{}>", email.to))?;
        let (code, _) = read_reply(stream)?;
        if code != 250 {
            return Err(format!("RCPT TO: expected 250, got {}", code));
        }

        // 6. DATA
        write_line(stream, "DATA")?;
        let (code, _) = read_reply(stream)?;
        if code != 354 {
            return Err(format!("DATA: expected 354, got {}", code));
        }

        // 6a. Message body + end-of-data marker
        let body = email.to_rfc5322();
        stream.write_all(body.as_bytes())?;
        stream.write_all(b".\r\n")?;
        let (code, _) = read_reply(stream)?;
        if code != 250 {
            return Err(format!("end of DATA: expected 250, got {}", code));
        }

        // 7. QUIT (best-effort; don't fail the send if the server is rude)
        write_line(stream, "QUIT")?;
        let _ = read_reply(stream);

        Ok(())
    }
}

/// Read one SMTP reply, handling multi-line `250-foo / 250 bar` responses.
fn read_reply(stream: &mut dyn SmtpStream) -> Result<(u16, String), String> {
    let mut full = String::new();
    loop {
        let line = stream.read_line()?;
        if line.len() < 3 {
            return Err(format!("malformed reply: {:?}", line));
        }
        let code: u16 = line[..3]
            .parse()
            .map_err(|_| format!("bad reply code: {:?}", line))?;
        let cont = line.len() >= 4 && line.as_bytes()[3] == b'-';
        if !full.is_empty() {
            full.push('\n');
        }
        full.push_str(&line);
        if !cont {
            return Ok((code, full));
        }
    }
}

fn write_line(stream: &mut dyn SmtpStream, line: &str) -> Result<(), String> {
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\r\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Email serialization ---

    fn sample() -> Email {
        Email {
            from: "me@gmail.com".to_string(),
            to: "you@example.com".to_string(),
            subject: "hi".to_string(),
            body: "line1\nline2".to_string(),
        }
    }

    #[test]
    fn rfc5322_includes_required_headers() {
        let s = sample().to_rfc5322();
        assert!(s.contains("From: me@gmail.com\r\n"));
        assert!(s.contains("To: you@example.com\r\n"));
        assert!(s.contains("Subject: hi\r\n"));
        assert!(s.contains("MIME-Version: 1.0\r\n"));
        assert!(s.contains("Content-Type: text/plain; charset=UTF-8\r\n"));
    }

    #[test]
    fn rfc5322_separates_headers_from_body_with_blank_line() {
        let s = sample().to_rfc5322();
        // The blank line is "\r\n\r\n" — the trailing CRLF of the last
        // header followed by the empty separator line.
        assert!(s.contains("charset=UTF-8\r\n\r\nline1\r\n"));
    }

    #[test]
    fn rfc5322_body_lines_are_crlf_terminated() {
        let s = sample().to_rfc5322();
        assert!(s.ends_with("line1\r\nline2\r\n"));
    }

    #[test]
    fn rfc5322_dot_stuffs_lines_starting_with_dot() {
        let email = Email {
            from: "a@b".into(),
            to: "c@d".into(),
            subject: "s".into(),
            body: ".secret\nnormal\n.also dotted".into(),
        };
        let s = email.to_rfc5322();
        assert!(s.contains("\r\n..secret\r\n"));
        assert!(s.contains("\r\nnormal\r\n"));
        assert!(s.ends_with("\r\n..also dotted\r\n"));
    }

    #[test]
    fn rfc5322_strips_existing_carriage_returns() {
        let email = Email {
            from: "a@b".into(),
            to: "c@d".into(),
            subject: "s".into(),
            // Caller passed CRLF — we should not double up to CRCRLF.
            body: "one\r\ntwo".into(),
        };
        let s = email.to_rfc5322();
        assert!(s.ends_with("one\r\ntwo\r\n"));
        assert!(!s.contains("\r\r"));
    }

    #[test]
    fn rfc5322_does_not_include_terminator() {
        // The session is responsible for the trailing ".\r\n" marker.
        let s = sample().to_rfc5322();
        assert!(!s.contains("\r\n.\r\n"));
    }

    // --- base64 ---

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn base64_one_byte() {
        assert_eq!(base64_encode(b"f"), "Zg==");
    }

    #[test]
    fn base64_two_bytes() {
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn base64_three_bytes() {
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn base64_longer_string() {
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_typical_email_address() {
        // What AUTH LOGIN would actually send
        assert_eq!(
            base64_encode(b"user@gmail.com"),
            "dXNlckBnbWFpbC5jb20="
        );
    }

    // --- MockSmtpStream ---

    #[test]
    fn mock_returns_queued_lines_in_order() {
        let mut s = MockSmtpStream::new();
        s.push_line("220 ready").push_line("250 OK");
        assert_eq!(s.read_line().unwrap(), "220 ready");
        assert_eq!(s.read_line().unwrap(), "250 OK");
        assert!(s.is_drained());
    }

    #[test]
    fn mock_records_writes() {
        let mut s = MockSmtpStream::new();
        s.write_all(b"EHLO me\r\n").unwrap();
        s.write_all(b"QUIT\r\n").unwrap();
        assert_eq!(s.written_str(), "EHLO me\r\nQUIT\r\n");
    }

    #[test]
    fn mock_read_after_drain_errors() {
        let mut s = MockSmtpStream::new();
        assert!(s.read_line().is_err());
    }

    // --- read_reply ---

    #[test]
    fn read_reply_single_line() {
        let mut s = MockSmtpStream::new();
        s.push_line("220 smtp.gmail.com ready");
        let (code, full) = read_reply(&mut s).unwrap();
        assert_eq!(code, 220);
        assert_eq!(full, "220 smtp.gmail.com ready");
    }

    #[test]
    fn read_reply_multi_line() {
        let mut s = MockSmtpStream::new();
        s.push_line("250-smtp.gmail.com")
            .push_line("250-SIZE 35882577")
            .push_line("250-AUTH LOGIN PLAIN")
            .push_line("250 STARTTLS");
        let (code, full) = read_reply(&mut s).unwrap();
        assert_eq!(code, 250);
        assert!(full.contains("STARTTLS"));
        assert!(full.contains("SIZE"));
    }

    #[test]
    fn read_reply_malformed() {
        let mut s = MockSmtpStream::new();
        s.push_line("xx");
        assert!(read_reply(&mut s).is_err());
    }

    // --- SmtpSession happy path ---

    /// Build a stream pre-loaded with a successful SMTP server transcript.
    fn happy_server() -> MockSmtpStream {
        let mut s = MockSmtpStream::new();
        s.push_line("220 smtp.gmail.com ready")
            // EHLO reply (multi-line)
            .push_line("250-smtp.gmail.com Hello")
            .push_line("250-AUTH LOGIN PLAIN")
            .push_line("250 OK")
            // AUTH LOGIN
            .push_line("334 VXNlcm5hbWU6")
            // username accepted
            .push_line("334 UGFzc3dvcmQ6")
            // password accepted
            .push_line("235 2.7.0 Accepted")
            // MAIL FROM
            .push_line("250 2.1.0 OK")
            // RCPT TO
            .push_line("250 2.1.5 OK")
            // DATA
            .push_line("354 Go ahead")
            // end of DATA
            .push_line("250 2.0.0 OK queued")
            // QUIT
            .push_line("221 2.0.0 closing");
        s
    }

    #[test]
    fn session_happy_path_succeeds() {
        let mut s = happy_server();
        let email = Email {
            from: "me@gmail.com".into(),
            to: "you@example.com".into(),
            subject: "hi".into(),
            body: "hello".into(),
        };
        SmtpSession::send(&mut s, "dynatac.local", "me@gmail.com", "abcd efgh ijkl mnop", &email)
            .expect("send should succeed");
        assert!(s.is_drained(), "every server line should have been consumed");
    }

    #[test]
    fn session_writes_expected_command_sequence() {
        let mut s = happy_server();
        let email = Email {
            from: "me@gmail.com".into(),
            to: "you@example.com".into(),
            subject: "hi".into(),
            body: "hello".into(),
        };
        SmtpSession::send(&mut s, "dynatac.local", "me@gmail.com", "pass", &email).unwrap();
        let written = s.written_str();

        assert!(written.contains("EHLO dynatac.local\r\n"));
        assert!(written.contains("AUTH LOGIN\r\n"));
        // base64 of "me@gmail.com" then "pass"
        assert!(written.contains(&format!("{}\r\n", base64_encode(b"me@gmail.com"))));
        assert!(written.contains(&format!("{}\r\n", base64_encode(b"pass"))));
        assert!(written.contains("MAIL FROM:<me@gmail.com>\r\n"));
        assert!(written.contains("RCPT TO:<you@example.com>\r\n"));
        assert!(written.contains("DATA\r\n"));
        // Body + terminator
        assert!(written.contains("Subject: hi\r\n"));
        assert!(written.contains("\r\nhello\r\n.\r\n"));
        assert!(written.ends_with("QUIT\r\n"));
    }

    #[test]
    fn session_command_order_is_correct() {
        let mut s = happy_server();
        let email = Email {
            from: "me@gmail.com".into(),
            to: "you@example.com".into(),
            subject: "hi".into(),
            body: "hello".into(),
        };
        SmtpSession::send(&mut s, "dynatac.local", "me@gmail.com", "pass", &email).unwrap();
        let w = s.written_str();
        // EHLO must come before AUTH; AUTH before MAIL; MAIL before RCPT;
        // RCPT before DATA; DATA before QUIT.
        let i_ehlo = w.find("EHLO").unwrap();
        let i_auth = w.find("AUTH LOGIN").unwrap();
        let i_mail = w.find("MAIL FROM").unwrap();
        let i_rcpt = w.find("RCPT TO").unwrap();
        let i_data = w.find("DATA\r\n").unwrap();
        let i_quit = w.find("QUIT").unwrap();
        assert!(i_ehlo < i_auth);
        assert!(i_auth < i_mail);
        assert!(i_mail < i_rcpt);
        assert!(i_rcpt < i_data);
        assert!(i_data < i_quit);
    }

    // --- Sad paths ---

    fn email_with(body: &str) -> Email {
        Email {
            from: "me@gmail.com".into(),
            to: "you@example.com".into(),
            subject: "hi".into(),
            body: body.into(),
        }
    }

    #[test]
    fn session_bad_greeting_errors() {
        let mut s = MockSmtpStream::new();
        s.push_line("554 not today");
        let err = SmtpSession::send(&mut s, "h", "u", "p", &email_with("body"))
            .unwrap_err();
        assert!(err.contains("greeting"));
        assert!(err.contains("554"));
    }

    #[test]
    fn session_auth_rejected() {
        let mut s = MockSmtpStream::new();
        s.push_line("220 ready")
            .push_line("250 OK")
            .push_line("334 VXNlcm5hbWU6")
            .push_line("334 UGFzc3dvcmQ6")
            .push_line("535 5.7.8 Username and Password not accepted");
        let err = SmtpSession::send(&mut s, "h", "u", "p", &email_with("body"))
            .unwrap_err();
        assert!(err.contains("AUTH password"));
        assert!(err.contains("535"));
    }

    #[test]
    fn session_rcpt_rejected() {
        let mut s = MockSmtpStream::new();
        s.push_line("220 ready")
            .push_line("250 OK")
            .push_line("334 VXNlcm5hbWU6")
            .push_line("334 UGFzc3dvcmQ6")
            .push_line("235 OK")
            .push_line("250 OK") // MAIL FROM
            .push_line("550 No such user");
        let err = SmtpSession::send(&mut s, "h", "u", "p", &email_with("body"))
            .unwrap_err();
        assert!(err.contains("RCPT TO"));
        assert!(err.contains("550"));
    }

    #[test]
    fn session_data_rejected_after_body() {
        let mut s = MockSmtpStream::new();
        s.push_line("220 ready")
            .push_line("250 OK")
            .push_line("334 VXNlcm5hbWU6")
            .push_line("334 UGFzc3dvcmQ6")
            .push_line("235 OK")
            .push_line("250 OK")
            .push_line("250 OK")
            .push_line("354 go ahead")
            .push_line("552 too big");
        let err = SmtpSession::send(&mut s, "h", "u", "p", &email_with("body"))
            .unwrap_err();
        assert!(err.contains("end of DATA"));
        assert!(err.contains("552"));
    }

    #[test]
    fn session_stream_read_error_propagates() {
        // Empty mock — first read_line errors with "no more server lines".
        let mut s = MockSmtpStream::new();
        let err = SmtpSession::send(&mut s, "h", "u", "p", &email_with("body"))
            .unwrap_err();
        assert!(err.contains("no more"));
    }

    #[test]
    fn session_dot_stuffs_body_lines() {
        let mut s = happy_server();
        let email = email_with(".secret message\nnormal");
        SmtpSession::send(&mut s, "h", "u", "p", &email).unwrap();
        let w = s.written_str();
        // The leading dot must be doubled in the wire form.
        assert!(w.contains("\r\n..secret message\r\n"));
        // The terminator is still recognizable.
        assert!(w.contains("\r\nnormal\r\n.\r\n"));
    }
}
