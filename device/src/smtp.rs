//! Real TLS-wrapped SMTP stream + factory.
//!
//! Wraps `esp_idf_svc::tls::EspTls` (mbedtls under the hood) and exposes
//! it through the [`SmtpStream`] trait so the pure-logic
//! [`dynatac_core::email::SmtpSession`] state machine can drive it.
//!
//! Driver invariants:
//!   - WiFi must be connected before `open()` is called.
//!   - Each call to `open()` discards any previously-returned stream and
//!     dials a fresh TLS connection. The factory only ever holds one
//!     live stream at a time, matching the trait's contract in
//!     `dynatac_core::email::SmtpStreamFactory`.
//!   - The CA bundle that ships with esp-idf is used (same one already
//!     wired up in `device/src/http.rs`), so no certificates are stored
//!     in app code.

use std::collections::VecDeque;

use dynatac_core::email::{SmtpStream, SmtpStreamFactory};
use esp_idf_svc::tls::{Config, EspTls, InternalSocket};

/// One TLS-wrapped SMTP connection.
pub struct EspSmtpStream {
    tls: EspTls<InternalSocket>,
    /// Bytes received from the server but not yet consumed by `read_line`.
    /// Used because `EspTls::read` returns whatever chunk arrives, not a
    /// line at a time — we need to buffer and split on CRLF ourselves.
    rx: VecDeque<u8>,
}

impl EspSmtpStream {
    fn connect(host: &str, port: u16) -> Result<Self, String> {
        let mut tls = EspTls::new().map_err(|e| format!("EspTls::new: {:?}", e))?;
        // Default Config has `use_crt_bundle_attach = true` (under the
        // certificate-bundle cfg), giving us Mozilla's CA bundle for free.
        let cfg = Config::new();
        tls.connect(host, port, &cfg)
            .map_err(|e| format!("tls connect: {:?}", e))?;
        Ok(Self {
            tls,
            rx: VecDeque::new(),
        })
    }

    /// Read more bytes from the TLS socket into the rx buffer.
    /// Returns an error if the connection is closed.
    fn fill_buffer(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 512];
        let n = self
            .tls
            .read(&mut buf)
            .map_err(|e| format!("tls read: {:?}", e))?;
        if n == 0 {
            return Err("connection closed by peer".to_string());
        }
        self.rx.extend(&buf[..n]);
        Ok(())
    }
}

impl SmtpStream for EspSmtpStream {
    fn read_line(&mut self) -> Result<String, String> {
        // Read bytes until we see CRLF, returning the line minus the CRLF.
        // Lone CR is treated as a literal byte; lone LF is also accepted as
        // a line terminator (defensive — Gmail uses CRLF strictly, but if
        // we ever talk to a less rigorous server it'll still work).
        let mut line: Vec<u8> = Vec::new();
        loop {
            while let Some(&b) = self.rx.front() {
                self.rx.pop_front();
                match b {
                    b'\r' => {
                        // Need to peek the next byte to confirm CRLF.
                        if self.rx.is_empty() {
                            self.fill_buffer()?;
                        }
                        if self.rx.front() == Some(&b'\n') {
                            self.rx.pop_front();
                            return String::from_utf8(line)
                                .map_err(|e| format!("utf8 in reply: {}", e));
                        }
                        // Bare CR — keep it as part of the line.
                        line.push(b'\r');
                    }
                    b'\n' => {
                        return String::from_utf8(line)
                            .map_err(|e| format!("utf8 in reply: {}", e));
                    }
                    other => line.push(other),
                }
            }
            self.fill_buffer()?;
        }
    }

    fn write_all(&mut self, data: &[u8]) -> Result<(), String> {
        self.tls
            .write_all(data)
            .map_err(|e| format!("tls write: {:?}", e))
    }
}

/// Factory that produces fresh `EspSmtpStream`s on demand.
pub struct EspSmtpStreamFactory {
    current: Option<EspSmtpStream>,
}

impl EspSmtpStreamFactory {
    pub fn new() -> Self {
        Self { current: None }
    }
}

impl Default for EspSmtpStreamFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl SmtpStreamFactory for EspSmtpStreamFactory {
    fn open(&mut self, host: &str, port: u16) -> Result<&mut dyn SmtpStream, String> {
        // Drop any previous stream first so the old TLS context is freed
        // before we allocate a new one.
        self.current = None;
        let stream = EspSmtpStream::connect(host, port)?;
        self.current = Some(stream);
        Ok(self.current.as_mut().unwrap() as &mut dyn SmtpStream)
    }
}
