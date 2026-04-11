//! Cellular modem (4G LTE) driver trait, shared types, and mock.
//!
//! The [`Modem`] trait is the high-level interface to an AT-commandable
//! cellular modem. The hardware implementation lives in
//! `device/src/modem.rs` and wraps the A7682E attached to UART1 of the
//! ESP32-S3. [`MockModem`] provides an in-memory double for host-side
//! tests of code that depends on the trait.
//!
//! The low-level AT response parser is in the [`at`] submodule and is
//! independently testable against recorded byte fixtures.
//!
//! Driver invariants:
//!   - [`Modem::power_on`] runs the hardware power-on sequence and blocks
//!     until the modem responds to `AT` with `OK`. On return, the modem
//!     is powered and in command mode, but *not* necessarily SIM-ready
//!     or network-registered.
//!   - [`Modem::power_off`] runs the hardware shutdown sequence. After
//!     it returns, [`Modem::is_powered`] is `false` and no further
//!     commands may be sent until [`Modem::power_on`] is called again.
//!   - [`Modem::send_raw`] blocks until a final result code is received
//!     (`OK`, `ERROR`, `+CME ERROR`, `+CMS ERROR`) or the command times
//!     out. It never returns while the modem is in data mode.
//!   - All methods return [`ModemError::NotPowered`] if called while the
//!     modem is powered off.

pub mod at;

pub use at::{classify, AtEvent, AtParser, LineClass, Urc};

/// SIM card status, as derived from `AT+CPIN?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimStatus {
    /// SIM is ready for use (`+CPIN: READY`).
    Ready,
    /// SIM requires a PIN or PUK (`+CPIN: SIM PIN` / `SIM PUK`).
    Locked,
    /// No SIM detected, or SIM error.
    NotReady,
    /// The response couldn't be parsed.
    Unknown,
}

/// Network registration status, as derived from `AT+CREG?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationStatus {
    /// Not registered, not searching.
    NotRegistered,
    /// Registered to the home network.
    RegisteredHome,
    /// Searching for a network.
    Searching,
    /// Registration denied by the network.
    Denied,
    /// Registered while roaming.
    Roaming,
    /// The response couldn't be parsed.
    Unknown,
}

impl RegistrationStatus {
    /// True if the modem is attached to a network (home or roaming).
    pub fn is_registered(&self) -> bool {
        matches!(
            self,
            RegistrationStatus::RegisteredHome | RegistrationStatus::Roaming
        )
    }

    /// Parse the `<stat>` field from `+CREG: <n>,<stat>...` (also valid
    /// for `+CEREG:` and `+CGREG:` — they share the encoding from
    /// 3GPP TS 27.007).
    pub fn from_creg_stat(stat: i32) -> Self {
        match stat {
            0 => RegistrationStatus::NotRegistered,
            1 => RegistrationStatus::RegisteredHome,
            2 => RegistrationStatus::Searching,
            3 => RegistrationStatus::Denied,
            5 => RegistrationStatus::Roaming,
            _ => RegistrationStatus::Unknown,
        }
    }
}

/// Combine two `RegistrationStatus` values (e.g. from `+CREG?` and
/// `+CEREG?`) into the more informative one. Used to summarise modems
/// that report both legacy and LTE registration separately — being
/// registered on *either* radio access technology counts as registered.
pub fn prefer_registered(a: RegistrationStatus, b: RegistrationStatus) -> RegistrationStatus {
    fn rank(r: &RegistrationStatus) -> i32 {
        match r {
            RegistrationStatus::RegisteredHome => 5,
            RegistrationStatus::Roaming => 4,
            RegistrationStatus::Searching => 3,
            RegistrationStatus::Denied => 2,
            RegistrationStatus::NotRegistered => 1,
            RegistrationStatus::Unknown => 0,
        }
    }
    if rank(&a) >= rank(&b) {
        a
    } else {
        b
    }
}

/// Parse `+CSQ: <rssi>,<ber>` into a signal strength in dBm.
///
/// RSSI values in `AT+CSQ` are reported as an index:
///   - 0   → -113 dBm or less
///   - 1   → -111 dBm
///   - 2..=30 → -109 .. -53 dBm (in 2 dBm steps)
///   - 31  → -51 dBm or greater
///   - 99  → unknown / not detectable
pub fn rssi_index_to_dbm(rssi: i32) -> Option<i32> {
    match rssi {
        0 => Some(-113),
        1..=30 => Some(-113 + 2 * rssi),
        31 => Some(-51),
        _ => None,
    }
}

/// Snapshot of the modem's status at a point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModemStatus {
    /// Whether the modem responded to `AT` during the most recent status query.
    pub responsive: bool,
    pub sim: SimStatus,
    pub registration: RegistrationStatus,
    /// Signal strength in dBm, or `None` if unknown / not detectable.
    pub signal_dbm: Option<i32>,
}

/// Errors that can be returned by a [`Modem`] implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModemError {
    /// Underlying transport error (UART I/O, etc.).
    Io(String),
    /// The modem did not return a final result code within the command timeout.
    Timeout,
    /// The modem responded with a plain `ERROR`.
    Error,
    /// The modem responded with `+CME ERROR: <code>`.
    CmeError(i32),
    /// The modem responded with `+CMS ERROR: <code>`.
    CmsError(i32),
    /// An operation was attempted while the modem was powered off.
    NotPowered,
    /// An AT-command or SMS operation was attempted while a data
    /// session is active. The modem's UART is currently owned by the
    /// PPP stack — call `disable_data` first to reclaim command mode.
    DataActive,
}

impl ModemError {
    /// Human-readable one-liner suitable for surfacing to the shell.
    pub fn display(&self) -> String {
        match self {
            ModemError::Io(s) => format!("io error: {}", s),
            ModemError::Timeout => "modem timeout".to_string(),
            ModemError::Error => "modem returned ERROR".to_string(),
            ModemError::CmeError(c) => format!("CME ERROR {}", c),
            ModemError::CmsError(c) => format!("CMS ERROR {}", c),
            ModemError::NotPowered => "modem is off".to_string(),
            ModemError::DataActive => "modem is in data mode".to_string(),
        }
    }
}

/// High-level interface to a cellular modem.
pub trait Modem {
    /// Power on the modem and wait until it responds to `AT` with `OK`.
    fn power_on(&mut self) -> Result<(), ModemError>;
    /// Power off the modem.
    fn power_off(&mut self) -> Result<(), ModemError>;
    /// Whether the modem is currently powered on (from this driver's
    /// point of view — may be out of sync with the hardware if power
    /// was cycled externally).
    fn is_powered(&self) -> bool;
    /// Query the modem's SIM, registration, and signal status.
    fn status(&mut self) -> Result<ModemStatus, ModemError>;
    /// Send a raw AT command (without trailing `\r`). Returns the
    /// information lines that preceded the final result code, or a
    /// classified error. Command echo is stripped automatically.
    fn send_raw(&mut self, cmd: &str) -> Result<Vec<String>, ModemError>;
    /// Send a command that solicits a `> ` prompt, then write `body`,
    /// then wait for a final result code. Used for `AT+CMGS` and
    /// similar prompt-based protocols.
    ///
    /// The caller is responsible for any command-specific terminator at
    /// the end of `body` (e.g. Ctrl-Z (`0x1A`) for SMS). The body is
    /// written to the modem verbatim — no escaping or transformation.
    ///
    /// Implementations should use a longer overall timeout than
    /// `send_raw` (e.g. 60 s) since over-the-air SMS delivery can be
    /// slow on weak signal.
    fn send_with_body(&mut self, cmd: &str, body: &[u8]) -> Result<Vec<String>, ModemError>;
    /// Bring up a cellular data session using the given APN. Blocks
    /// until either the PPP link has an IP address or the bring-up
    /// times out. Requires the modem to be powered on and (ideally)
    /// registered to a network — the caller should verify registration
    /// separately if that matters.
    ///
    /// While a data session is active, `send_raw` and `send_with_body`
    /// return [`ModemError::DataActive`] — the UART is owned by the
    /// PPP stack. Call `disable_data` to return to command mode.
    fn enable_data(&mut self, apn: &str) -> Result<(), ModemError>;
    /// Tear down an active data session, returning the modem to
    /// command mode. No-op if not currently in data mode.
    fn disable_data(&mut self) -> Result<(), ModemError>;
    /// Whether a data session is currently active.
    fn is_data_active(&self) -> bool;
}

/// In-memory test double for [`Modem`].
///
/// Tracks power state and returns scripted responses for `send_raw` and
/// `send_with_body`. Also records the bodies passed to `send_with_body`
/// so tests can assert they were encoded correctly.
///
/// The default status is "registered, SIM ready, strong signal" so
/// programs that only inspect `status()` work without setup.
pub struct MockModem {
    powered: bool,
    status: ModemStatus,
    raw_responses: std::collections::HashMap<String, Result<Vec<String>, ModemError>>,
    body_responses: std::collections::HashMap<String, Result<Vec<String>, ModemError>>,
    data_active: bool,
    /// APN passed to the last successful `enable_data` call, for test
    /// assertions. `None` if data has never been enabled.
    pub last_apn: Option<String>,
    /// Forced error for the next `enable_data` call, to simulate modem
    /// failures. Consumed on use.
    pub enable_data_error: Option<ModemError>,
    /// All commands sent via `send_raw`, in order.
    pub raw_log: Vec<String>,
    /// All `(cmd, body)` pairs sent via `send_with_body`, in order.
    pub body_log: Vec<(String, Vec<u8>)>,
}

impl MockModem {
    pub fn new() -> Self {
        Self {
            powered: false,
            status: ModemStatus {
                responsive: true,
                sim: SimStatus::Ready,
                registration: RegistrationStatus::RegisteredHome,
                signal_dbm: Some(-71),
            },
            raw_responses: std::collections::HashMap::new(),
            body_responses: std::collections::HashMap::new(),
            data_active: false,
            last_apn: None,
            enable_data_error: None,
            raw_log: Vec::new(),
            body_log: Vec::new(),
        }
    }

    /// Override the status returned by `status()`.
    pub fn set_status(&mut self, status: ModemStatus) {
        self.status = status;
    }

    /// Register a canned response for a raw command.
    pub fn on_raw(&mut self, cmd: &str, response: Result<Vec<String>, ModemError>) {
        self.raw_responses.insert(cmd.to_string(), response);
    }

    /// Register a canned response for a `send_with_body` command. The body
    /// is not part of the key — the script reacts based on the command
    /// alone, mirroring how the real modem responds the same way regardless
    /// of body content for valid commands.
    pub fn on_with_body(&mut self, cmd: &str, response: Result<Vec<String>, ModemError>) {
        self.body_responses.insert(cmd.to_string(), response);
    }
}

impl Default for MockModem {
    fn default() -> Self {
        Self::new()
    }
}

impl Modem for MockModem {
    fn power_on(&mut self) -> Result<(), ModemError> {
        self.powered = true;
        self.status.responsive = true;
        Ok(())
    }

    fn power_off(&mut self) -> Result<(), ModemError> {
        // Mirror the real EspA7682EModem: tear down any active data
        // session before powering off so tests reflect the invariant
        // that a powered-off modem has no data session.
        self.data_active = false;
        self.powered = false;
        self.status.responsive = false;
        Ok(())
    }

    fn is_powered(&self) -> bool {
        self.powered
    }

    fn status(&mut self) -> Result<ModemStatus, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        Ok(self.status.clone())
    }

    fn send_raw(&mut self, cmd: &str) -> Result<Vec<String>, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        if self.data_active {
            return Err(ModemError::DataActive);
        }
        self.raw_log.push(cmd.to_string());
        match self.raw_responses.get(cmd) {
            Some(r) => r.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn send_with_body(&mut self, cmd: &str, body: &[u8]) -> Result<Vec<String>, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        if self.data_active {
            return Err(ModemError::DataActive);
        }
        self.body_log.push((cmd.to_string(), body.to_vec()));
        match self.body_responses.get(cmd) {
            Some(r) => r.clone(),
            None => Ok(Vec::new()),
        }
    }

    fn enable_data(&mut self, apn: &str) -> Result<(), ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        if let Some(err) = self.enable_data_error.take() {
            return Err(err);
        }
        self.data_active = true;
        self.last_apn = Some(apn.to_string());
        Ok(())
    }

    fn disable_data(&mut self) -> Result<(), ModemError> {
        self.data_active = false;
        Ok(())
    }

    fn is_data_active(&self) -> bool {
        self.data_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rssi_index_unknown() {
        assert_eq!(rssi_index_to_dbm(99), None);
        assert_eq!(rssi_index_to_dbm(50), None);
    }

    #[test]
    fn rssi_index_extremes() {
        assert_eq!(rssi_index_to_dbm(0), Some(-113));
        assert_eq!(rssi_index_to_dbm(31), Some(-51));
    }

    #[test]
    fn rssi_index_midrange() {
        // 15 → -113 + 30 = -83 dBm (a "good" signal).
        assert_eq!(rssi_index_to_dbm(15), Some(-83));
    }

    #[test]
    fn creg_stat_mapping() {
        assert_eq!(
            RegistrationStatus::from_creg_stat(0),
            RegistrationStatus::NotRegistered
        );
        assert_eq!(
            RegistrationStatus::from_creg_stat(1),
            RegistrationStatus::RegisteredHome
        );
        assert_eq!(
            RegistrationStatus::from_creg_stat(2),
            RegistrationStatus::Searching
        );
        assert_eq!(
            RegistrationStatus::from_creg_stat(3),
            RegistrationStatus::Denied
        );
        assert_eq!(
            RegistrationStatus::from_creg_stat(5),
            RegistrationStatus::Roaming
        );
        assert_eq!(
            RegistrationStatus::from_creg_stat(42),
            RegistrationStatus::Unknown
        );
    }

    #[test]
    fn prefer_registered_picks_registered_over_searching() {
        assert_eq!(
            prefer_registered(
                RegistrationStatus::RegisteredHome,
                RegistrationStatus::Searching,
            ),
            RegistrationStatus::RegisteredHome
        );
        assert_eq!(
            prefer_registered(
                RegistrationStatus::Searching,
                RegistrationStatus::RegisteredHome,
            ),
            RegistrationStatus::RegisteredHome
        );
    }

    #[test]
    fn prefer_registered_picks_roaming_over_unregistered() {
        // Common LTE-only case: CEREG=roaming, CREG=not registered.
        assert_eq!(
            prefer_registered(
                RegistrationStatus::Roaming,
                RegistrationStatus::NotRegistered,
            ),
            RegistrationStatus::Roaming
        );
    }

    #[test]
    fn prefer_registered_picks_home_over_roaming() {
        assert_eq!(
            prefer_registered(
                RegistrationStatus::RegisteredHome,
                RegistrationStatus::Roaming,
            ),
            RegistrationStatus::RegisteredHome
        );
    }

    #[test]
    fn prefer_registered_falls_back_to_more_informative_unregistered() {
        // Searching is more informative than Unknown.
        assert_eq!(
            prefer_registered(
                RegistrationStatus::Unknown,
                RegistrationStatus::Searching,
            ),
            RegistrationStatus::Searching
        );
        // Denied is more informative than NotRegistered (tells the user
        // why they're stuck).
        assert_eq!(
            prefer_registered(
                RegistrationStatus::Denied,
                RegistrationStatus::NotRegistered,
            ),
            RegistrationStatus::Denied
        );
    }

    #[test]
    fn is_registered_is_true_for_home_and_roaming() {
        assert!(RegistrationStatus::RegisteredHome.is_registered());
        assert!(RegistrationStatus::Roaming.is_registered());
        assert!(!RegistrationStatus::NotRegistered.is_registered());
        assert!(!RegistrationStatus::Searching.is_registered());
    }

    #[test]
    fn mock_starts_powered_off() {
        let m = MockModem::new();
        assert!(!m.is_powered());
    }

    #[test]
    fn mock_power_on_off() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        assert!(m.is_powered());
        m.power_off().unwrap();
        assert!(!m.is_powered());
    }

    #[test]
    fn mock_status_requires_power() {
        let mut m = MockModem::new();
        assert_eq!(m.status().unwrap_err(), ModemError::NotPowered);
        m.power_on().unwrap();
        assert!(m.status().is_ok());
    }

    #[test]
    fn mock_send_raw_requires_power() {
        let mut m = MockModem::new();
        assert_eq!(m.send_raw("AT").unwrap_err(), ModemError::NotPowered);
    }

    #[test]
    fn mock_send_raw_returns_canned_response() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.on_raw("AT+CSQ", Ok(vec!["+CSQ: 20,99".to_string()]));
        assert_eq!(m.send_raw("AT+CSQ").unwrap(), vec!["+CSQ: 20,99"]);
    }

    #[test]
    fn mock_send_raw_unscripted_returns_empty() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        assert_eq!(m.send_raw("AT").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn mock_enable_data_requires_power() {
        let mut m = MockModem::new();
        assert_eq!(m.enable_data("everywhere").unwrap_err(), ModemError::NotPowered);
    }

    #[test]
    fn mock_enable_data_sets_active_and_records_apn() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        assert!(!m.is_data_active());
        m.enable_data("everywhere").unwrap();
        assert!(m.is_data_active());
        assert_eq!(m.last_apn.as_deref(), Some("everywhere"));
    }

    #[test]
    fn mock_enable_data_honors_scripted_error() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.enable_data_error = Some(ModemError::Timeout);
        assert_eq!(m.enable_data("everywhere").unwrap_err(), ModemError::Timeout);
        assert!(!m.is_data_active());
    }

    #[test]
    fn mock_send_raw_errors_while_data_active() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.enable_data("everywhere").unwrap();
        assert_eq!(m.send_raw("AT").unwrap_err(), ModemError::DataActive);
    }

    #[test]
    fn mock_send_with_body_errors_while_data_active() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.enable_data("everywhere").unwrap();
        assert_eq!(
            m.send_with_body("AT+CMGS=\"+1\"", b"x").unwrap_err(),
            ModemError::DataActive
        );
    }

    #[test]
    fn mock_disable_data_returns_to_command_mode() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.enable_data("everywhere").unwrap();
        m.disable_data().unwrap();
        assert!(!m.is_data_active());
        // AT commands work again.
        assert!(m.send_raw("AT").is_ok());
    }

    #[test]
    fn mock_disable_data_is_idempotent() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        // Never called enable_data — disable_data should still be Ok.
        assert!(m.disable_data().is_ok());
    }

    #[test]
    fn mock_send_with_body_records_command_and_body() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.on_with_body("AT+CMGS=\"+447\"", Ok(vec!["+CMGS: 42".into()]));
        let result = m.send_with_body("AT+CMGS=\"+447\"", b"hello\x1a").unwrap();
        assert_eq!(result, vec!["+CMGS: 42"]);
        assert_eq!(m.body_log.len(), 1);
        assert_eq!(m.body_log[0].0, "AT+CMGS=\"+447\"");
        assert_eq!(m.body_log[0].1, b"hello\x1a");
    }

    #[test]
    fn mock_send_with_body_requires_power() {
        let mut m = MockModem::new();
        assert_eq!(
            m.send_with_body("AT+CMGS=\"+1\"", b"x").unwrap_err(),
            ModemError::NotPowered
        );
    }

    #[test]
    fn mock_raw_log_records_commands_in_order() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.send_raw("AT").ok();
        m.send_raw("AT+CMGF=1").ok();
        // power_on doesn't go through send_raw on the mock, so the log
        // contains only what we explicitly sent.
        assert_eq!(m.raw_log, vec!["AT", "AT+CMGF=1"]);
    }

    #[test]
    fn mock_send_raw_canned_error() {
        let mut m = MockModem::new();
        m.power_on().unwrap();
        m.on_raw("AT+CMGS=\"+1234\"", Err(ModemError::CmsError(310)));
        assert_eq!(
            m.send_raw("AT+CMGS=\"+1234\"").unwrap_err(),
            ModemError::CmsError(310)
        );
    }

    #[test]
    fn error_display_strings() {
        assert_eq!(ModemError::NotPowered.display(), "modem is off");
        assert_eq!(ModemError::Timeout.display(), "modem timeout");
        assert_eq!(ModemError::CmeError(13).display(), "CME ERROR 13");
    }
}
