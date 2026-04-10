//! Real A7682E (4G LTE) modem driver on UART1.
//!
//! Wraps a [`UartDriver`] and the PWRKEY/POWER_EN GPIO pins, implementing
//! the [`Modem`] trait from `dynatac_core`. The low-level AT byte-stream
//! parser and line classifier live in `dynatac_core::modem::at` so they
//! can be tested on the host against recorded fixtures.
//!
//! Driver invariants:
//!   - On construction, POWER_EN is driven HIGH (module power rail on)
//!     and PWRKEY is driven LOW (released). The module is not yet
//!     powered on — that requires the PWRKEY pulse performed by
//!     `power_on`.
//!   - `power_on` runs a 1 s PWRKEY pulse and polls `AT` until the
//!     modem responds (or times out at 15 s). On success, command echo
//!     is disabled via `ATE0`.
//!   - `power_off` runs a 3 s PWRKEY pulse per the T-Deck-Pro example
//!     and A76xx datasheet.
//!   - `send_raw` is blocking: it drains any stale UART bytes, writes
//!     the command + `\r`, then reads bytes into the AT parser until a
//!     final result code is observed or the 5 s command timeout expires.
//!     Command echo is automatically discarded. Recognised URCs are
//!     logged at INFO and otherwise ignored (a proper URC channel is a
//!     future step).
//!   - On the T-Deck-Pro, the PWRKEY pin is inverted on-board: driving
//!     the ESP32 pin HIGH presses the modem's power button, LOW releases
//!     it. This matches the T-Deck-Pro Arduino examples.

use std::thread::sleep;
use std::time::{Duration, Instant};

use dynatac_core::modem::{
    classify, prefer_registered, rssi_index_to_dbm, AtEvent, AtParser, LineClass, Modem,
    ModemError, ModemStatus, RegistrationStatus, SimStatus,
};
use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, Output, PinDriver},
    uart::UartDriver,
};
use esp_idf_svc::sys::ESP_ERR_TIMEOUT;

/// Maximum time to wait for a single command's final result code.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum time to wait for the modem to become responsive after power-on.
const POWER_ON_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-read blocking tick budget. Small so the outer deadline drives
/// cancellation; the exact value depends on CONFIG_FREERTOS_HZ but is
/// only a polling granularity.
const READ_TICKS: u32 = 10;

pub struct EspA7682EModem<'d> {
    uart: UartDriver<'d>,
    pwrkey: PinDriver<'d, AnyOutputPin, Output>,
    // Held so the PinDriver outlives the modem and keeps POWER_EN driven
    // HIGH. Dropping it would release the pin and cut module power.
    #[allow(dead_code)]
    power_en: PinDriver<'d, AnyOutputPin, Output>,
    parser: AtParser,
    powered: bool,
}

impl<'d> EspA7682EModem<'d> {
    /// Construct the driver. The modem is *not* powered on; call
    /// [`Modem::power_on`] when you want to use it.
    pub fn new(
        uart: UartDriver<'d>,
        mut pwrkey: PinDriver<'d, AnyOutputPin, Output>,
        mut power_en: PinDriver<'d, AnyOutputPin, Output>,
    ) -> Self {
        // Enable the modem's power rail and ensure PWRKEY is released
        // (LOW, which on this board means "not pressing" the power key).
        let _ = power_en.set_high();
        let _ = pwrkey.set_low();
        Self {
            uart,
            pwrkey,
            power_en,
            parser: AtParser::new(),
            powered: false,
        }
    }

    /// Drive PWRKEY HIGH for `hold` (the "press"), bracketed by brief
    /// LOW states. On this board HIGH = pressing the modem's power key.
    fn pulse_pwrkey(&mut self, hold: Duration) -> Result<(), ModemError> {
        self.pwrkey
            .set_low()
            .map_err(|e| ModemError::Io(format!("pwrkey low: {:?}", e)))?;
        sleep(Duration::from_millis(10));
        self.pwrkey
            .set_high()
            .map_err(|e| ModemError::Io(format!("pwrkey high: {:?}", e)))?;
        sleep(hold);
        self.pwrkey
            .set_low()
            .map_err(|e| ModemError::Io(format!("pwrkey low: {:?}", e)))?;
        sleep(Duration::from_millis(10));
        Ok(())
    }

    /// Discard any bytes sitting in the UART RX buffer. Used before
    /// sending a command so stale modem chatter (URCs, boot banner)
    /// doesn't contaminate the response. Capped to avoid spinning
    /// forever if the modem is actively transmitting.
    fn drain_uart(&mut self) {
        let mut buf = [0u8; 256];
        for _ in 0..16 {
            match self.uart.read(&mut buf, 0) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }

    /// Write a command string followed by the AT terminator `\r`.
    fn write_command(&mut self, cmd: &str) -> Result<(), ModemError> {
        self.uart
            .write(cmd.as_bytes())
            .map_err(|e| ModemError::Io(format!("{:?}", e)))?;
        self.uart
            .write(b"\r")
            .map_err(|e| ModemError::Io(format!("{:?}", e)))?;
        Ok(())
    }

    /// Read bytes, feed the parser, and return when a final result code
    /// is observed or the deadline passes.
    fn read_until_final(&mut self, deadline: Instant) -> Result<Vec<String>, ModemError> {
        let mut info: Vec<String> = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            if Instant::now() >= deadline {
                return Err(ModemError::Timeout);
            }
            // The UART driver returns ESP_ERR_TIMEOUT when its blocking
            // read budget elapses with no bytes — that's normal idle on
            // the AT line, NOT a fatal I/O error. Treat it as "no data,
            // keep polling" so the outer `deadline` is what cancels.
            // Any other error code propagates as a real I/O failure.
            let n = match self.uart.read(&mut buf, READ_TICKS) {
                Ok(n) => n,
                Err(e) if e.code() == ESP_ERR_TIMEOUT => 0,
                Err(e) => return Err(ModemError::Io(format!("{:?}", e))),
            };
            if n == 0 {
                continue;
            }
            for event in self.parser.feed(&buf[..n]) {
                match event {
                    AtEvent::Prompt => {
                        // `send_raw` callers shouldn't be using commands
                        // that solicit a `> ` prompt — those need a
                        // dedicated path that writes the body + Ctrl-Z.
                        // Log and keep waiting so we don't hang.
                        log::warn!("unexpected `> ` prompt during send_raw");
                    }
                    AtEvent::Line(line) => match classify(&line) {
                        LineClass::Ok => return Ok(info),
                        LineClass::Error => return Err(ModemError::Error),
                        LineClass::CmeError(c) => return Err(ModemError::CmeError(c)),
                        LineClass::CmsError(c) => return Err(ModemError::CmsError(c)),
                        LineClass::NoCarrier => return Err(ModemError::Error),
                        LineClass::Urc(urc) => {
                            log::info!("modem URC: {:?}", urc);
                        }
                        LineClass::Echo => {
                            // Discard command echo.
                        }
                        LineClass::Info => info.push(line),
                    },
                }
            }
        }
    }

    /// Internal send with an explicit timeout, used by power-on probing
    /// where we want a shorter per-attempt deadline than the default.
    fn send_raw_with_timeout(
        &mut self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<Vec<String>, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        self.parser.reset();
        self.drain_uart();
        self.write_command(cmd)?;
        self.read_until_final(Instant::now() + timeout)
    }

    fn query_sim(&mut self) -> SimStatus {
        match self.send_raw("AT+CPIN?") {
            Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.trim().strip_prefix("+CPIN:") {
                        return match rest.trim() {
                            "READY" => SimStatus::Ready,
                            "SIM PIN" | "SIM PUK" | "PH-SIM PIN" | "SIM PIN2" | "SIM PUK2" => {
                                SimStatus::Locked
                            }
                            _ => SimStatus::NotReady,
                        };
                    }
                }
                SimStatus::Unknown
            }
            Err(_) => SimStatus::NotReady,
        }
    }

    /// Query a `+C(E)REG` registration command and parse the `<stat>` field.
    /// `prefix` is the expected response prefix, e.g. `"+CREG:"` or `"+CEREG:"`.
    fn query_reg_command(&mut self, cmd: &str, prefix: &str) -> RegistrationStatus {
        match self.send_raw(cmd) {
            Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.trim().strip_prefix(prefix) {
                        // Format: <n>,<stat>[,<lac>,<ci>[,<AcT>]]
                        let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
                        if parts.len() >= 2 {
                            if let Ok(stat) = parts[1].parse::<i32>() {
                                return RegistrationStatus::from_creg_stat(stat);
                            }
                        }
                    }
                }
                RegistrationStatus::Unknown
            }
            Err(_) => RegistrationStatus::Unknown,
        }
    }

    /// Query both legacy (`AT+CREG?`) and EPS/LTE (`AT+CEREG?`) registration.
    /// The A7682E is an LTE Cat-1 modem; on LTE-only carriers `CREG` will
    /// always be unregistered, so `CEREG` is the source of truth. We query
    /// both and return the more "registered" of the two so non-LTE carriers
    /// still work.
    fn query_registration(&mut self) -> RegistrationStatus {
        let lte = self.query_reg_command("AT+CEREG?", "+CEREG:");
        let legacy = self.query_reg_command("AT+CREG?", "+CREG:");
        prefer_registered(lte, legacy)
    }

    fn query_signal(&mut self) -> Option<i32> {
        match self.send_raw("AT+CSQ") {
            Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.trim().strip_prefix("+CSQ:") {
                        let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
                        if let Some(rssi_s) = parts.first() {
                            if let Ok(rssi) = rssi_s.parse::<i32>() {
                                return rssi_index_to_dbm(rssi);
                            }
                        }
                    }
                }
                None
            }
            Err(_) => None,
        }
    }
}

impl Modem for EspA7682EModem<'_> {
    fn power_on(&mut self) -> Result<(), ModemError> {
        if self.powered {
            return Ok(());
        }
        log::info!("A7682E power-on pulse");
        // 1000 ms press matches the T-Deck-Pro factory.ino retry timing
        // and is comfortably above the A76xx datasheet minimum (~100 ms).
        self.pulse_pwrkey(Duration::from_millis(1000))?;

        // The modem boot takes a few seconds. Poll AT until it responds.
        // `powered` is set tentatively so the inner send_raw_with_timeout
        // is allowed to run; it is cleared again on final timeout.
        let deadline = Instant::now() + POWER_ON_TIMEOUT;
        self.powered = true;
        loop {
            if Instant::now() >= deadline {
                self.powered = false;
                return Err(ModemError::Timeout);
            }
            match self.send_raw_with_timeout("AT", Duration::from_millis(500)) {
                Ok(_) => {
                    log::info!("A7682E responsive");
                    // Disable command echo. Parser tolerates echo, but
                    // turning it off keeps responses cleaner. Errors are
                    // non-fatal — some firmware rev doesn't echo anyway.
                    let _ = self.send_raw("ATE0");
                    return Ok(());
                }
                Err(ModemError::Timeout) | Err(ModemError::Error) => {
                    sleep(Duration::from_millis(200));
                }
                Err(e) => {
                    self.powered = false;
                    return Err(e);
                }
            }
        }
    }

    fn power_off(&mut self) -> Result<(), ModemError> {
        if !self.powered {
            return Ok(());
        }
        log::info!("A7682E power-off pulse");
        // >= 2.5 s press required per A76xx datasheet; 3 s matches the
        // T-Deck-Pro example.
        self.pulse_pwrkey(Duration::from_millis(3000))?;
        self.powered = false;
        self.parser.reset();
        Ok(())
    }

    fn is_powered(&self) -> bool {
        self.powered
    }

    fn status(&mut self) -> Result<ModemStatus, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        let responsive = self.send_raw("AT").is_ok();
        let sim = self.query_sim();
        let registration = self.query_registration();
        let signal_dbm = self.query_signal();
        Ok(ModemStatus {
            responsive,
            sim,
            registration,
            signal_dbm,
        })
    }

    fn send_raw(&mut self, cmd: &str) -> Result<Vec<String>, ModemError> {
        self.send_raw_with_timeout(cmd, COMMAND_TIMEOUT)
    }
}
