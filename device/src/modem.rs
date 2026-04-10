//! Real A7682E (4G LTE) modem driver on UART1.
//!
//! Wraps a [`UartDriver`] (shared via [`Arc`] so the PPP data path can
//! clone it without giving up command-mode access) and the PWRKEY/
//! POWER_EN GPIO pins. Implements the [`Modem`] trait from
//! `dynatac_core`.
//!
//! The modem has two operational phases:
//!
//!   - **Command mode.** `send_raw` / `send_with_body` / `status` work
//!     normally. The AT parser consumes UART RX bytes synchronously on
//!     the calling thread.
//!   - **Data mode.** A PPP session is active (set up by `enable_data`).
//!     The UART byte stream is owned by lwIP's PPP stack via an
//!     [`EspNetifDriver`]. AT commands return [`ModemError::DataActive`].
//!     A dedicated **RX pump thread** (spawned by `enable_data`, joined
//!     by `disable_data`) is the sole reader of UART RX bytes, feeding
//!     them into the PPP stack. It also owns the EspNetifDriver outright
//!     — this sidesteps having to share that type across threads, which
//!     it isn't designed for. The TX direction is handled by esp-idf's
//!     own hidden thread invoking the tx closure we gave to
//!     EspNetifDriver::new.
//!
//! # Lifetime workaround
//!
//! `std::thread::spawn` requires `'static` closures, but `UartDriver<'d>`
//! and `EspNetifDriver<'d, _>` carry the peripheral lifetime from
//! `main()`'s stack. We use `std::mem::transmute` to force-upgrade those
//! to `'static` at the point of spawning. This is sound because:
//!
//!   1. `main()` is an infinite event loop that never returns, so the
//!      `UartDriver` held by `EspA7682EModem::uart` effectively lives
//!      for the entire program run.
//!   2. `disable_data` joins the pump thread before any other cleanup,
//!      and the thread destructor (triggered by thread exit) drops the
//!      owned `EspNetifDriver` at that point.
//!   3. Therefore the thread can never outlive either the `UartDriver`
//!      or the `EspNetifDriver` it references.
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
//!     and A76xx datasheet. If a data session is active, it is torn
//!     down first.
//!   - `enable_data` blocks until either PPP negotiates an IP address
//!     or a 30 s deadline expires; in the meantime it pumps UART RX
//!     into the PPP stack so negotiation can make progress.
//!   - On the T-Deck-Pro, the PWRKEY pin is inverted on-board: driving
//!     the ESP32 pin HIGH presses the modem's power button, LOW releases
//!     it. This matches the T-Deck-Pro Arduino examples.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, sleep, JoinHandle};
use std::time::{Duration, Instant};

use dynatac_core::modem::{
    classify, prefer_registered, rssi_index_to_dbm, AtEvent, AtParser, LineClass, Modem,
    ModemError, ModemStatus, RegistrationStatus, SimStatus,
};
use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, Output, PinDriver},
    uart::UartDriver,
};
use esp_idf_svc::netif::{EspNetif, EspNetifDriver, NetifStack, PppConfiguration};
use esp_idf_svc::sys::ESP_ERR_TIMEOUT;

/// Maximum time to wait for a single command's final result code.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum time to wait for the modem to become responsive after power-on.
const POWER_ON_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum time to wait for the `> ` prompt after issuing a body-soliciting
/// command (e.g. `AT+CMGS`).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum time to wait for the final result code after writing a body.
/// SMS over-the-air delivery on a weak signal can take 30+ s.
const SEND_BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time to wait for the modem's `CONNECT` response after `ATD*99#`.
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time to wait for PPP to negotiate an IP address after `CONNECT`.
const PPP_UP_TIMEOUT: Duration = Duration::from_secs(30);
/// Per-read blocking tick budget for AT commands. Small so the outer
/// deadline drives cancellation; the exact value depends on
/// CONFIG_FREERTOS_HZ but is only a polling granularity.
const READ_TICKS: u32 = 10;

pub struct EspA7682EModem<'d> {
    uart: Arc<UartDriver<'d>>,
    pwrkey: PinDriver<'d, AnyOutputPin, Output>,
    // Held so the PinDriver outlives the modem and keeps POWER_EN driven
    // HIGH. Dropping it would release the pin and cut module power.
    #[allow(dead_code)]
    power_en: PinDriver<'d, AnyOutputPin, Output>,
    parser: AtParser,
    powered: bool,
    data_session: Option<DataSession>,
}

/// Handle to the background RX pump thread. Dropping or calling
/// `stop_and_join` on this ends the data session and releases the
/// EspNetifDriver the thread owns (which in turn drops our tx closure
/// and closes the PPP netif). Construction is in `enable_data`.
struct DataSession {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Newtype wrapper so we can move an [`EspNetifDriver`] into a
/// `std::thread::spawn` closure. The driver contains a raw
/// `*mut esp_netif_obj` pointer which is not `Send` by default, but
/// esp-idf's netif APIs are designed to be called from any task (the
/// lwIP tcpip thread, the driver's hidden tx thread, our pump thread,
/// etc.), so moving the handle across thread boundaries is sound.
///
/// We never hand out shared access to the inner driver; the pump
/// thread is the single owner, so `Sync` is not needed.
struct SendNetifDriver(EspNetifDriver<'static, EspNetif>);

// SAFETY: see type-level comment above.
unsafe impl Send for SendNetifDriver {}

impl DataSession {
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            // If the thread panicked, we've already torn down the PPP
            // session via its destructor — log but don't propagate.
            if let Err(e) = handle.join() {
                log::warn!("ppp rx thread panicked: {:?}", e);
            }
        }
    }
}

impl<'d> EspA7682EModem<'d> {
    /// Construct the driver. The modem is *not* powered on; call
    /// [`Modem::power_on`] when you want to use it.
    pub fn new(
        uart: UartDriver<'d>,
        mut pwrkey: PinDriver<'d, AnyOutputPin, Output>,
        mut power_en: PinDriver<'d, AnyOutputPin, Output>,
    ) -> Self {
        let _ = power_en.set_high();
        let _ = pwrkey.set_low();
        Self {
            uart: Arc::new(uart),
            pwrkey,
            power_en,
            parser: AtParser::new(),
            powered: false,
            data_session: None,
        }
    }

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

    fn drain_uart(&mut self) {
        let mut buf = [0u8; 256];
        for _ in 0..16 {
            match self.uart.read(&mut buf, 0) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }

    fn write_command(&mut self, cmd: &str) -> Result<(), ModemError> {
        self.uart
            .write(cmd.as_bytes())
            .map_err(|e| ModemError::Io(format!("{:?}", e)))?;
        self.uart
            .write(b"\r")
            .map_err(|e| ModemError::Io(format!("{:?}", e)))?;
        Ok(())
    }

    fn read_until_final(&mut self, deadline: Instant) -> Result<Vec<String>, ModemError> {
        let mut info: Vec<String> = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            if Instant::now() >= deadline {
                return Err(ModemError::Timeout);
            }
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
                        log::warn!("unexpected `> ` prompt during send_raw");
                    }
                    AtEvent::Line(line) => match classify(&line) {
                        LineClass::Ok => return Ok(info),
                        LineClass::Error => return Err(ModemError::Error),
                        LineClass::CmeError(c) => return Err(ModemError::CmeError(c)),
                        LineClass::CmsError(c) => return Err(ModemError::CmsError(c)),
                        LineClass::NoCarrier => return Err(ModemError::Error),
                        LineClass::Urc(urc) => log::info!("modem URC: {:?}", urc),
                        LineClass::Echo => {}
                        LineClass::Info => info.push(line),
                    },
                }
            }
        }
    }

    fn send_raw_with_timeout(
        &mut self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<Vec<String>, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        if self.data_session.is_some() {
            return Err(ModemError::DataActive);
        }
        self.parser.reset();
        self.drain_uart();
        self.write_command(cmd)?;
        self.read_until_final(Instant::now() + timeout)
    }

    fn read_until_prompt(&mut self, deadline: Instant) -> Result<(), ModemError> {
        let mut buf = [0u8; 256];
        loop {
            if Instant::now() >= deadline {
                return Err(ModemError::Timeout);
            }
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
                    AtEvent::Prompt => return Ok(()),
                    AtEvent::Line(line) => match classify(&line) {
                        LineClass::Error => return Err(ModemError::Error),
                        LineClass::CmeError(c) => return Err(ModemError::CmeError(c)),
                        LineClass::CmsError(c) => return Err(ModemError::CmsError(c)),
                        LineClass::NoCarrier => return Err(ModemError::Error),
                        LineClass::Urc(urc) => log::info!("modem URC: {:?}", urc),
                        _ => {}
                    },
                }
            }
        }
    }

    /// After sending `ATD*99#`, read bytes until we see a line starting
    /// with `CONNECT` (dial-up succeeded) or a final error code.
    fn wait_for_connect(&mut self, deadline: Instant) -> Result<(), ModemError> {
        let mut buf = [0u8; 256];
        loop {
            if Instant::now() >= deadline {
                log::warn!("modem: ATD*99# timed out waiting for CONNECT");
                return Err(ModemError::Timeout);
            }
            let n = match self.uart.read(&mut buf, READ_TICKS) {
                Ok(n) => n,
                Err(e) if e.code() == ESP_ERR_TIMEOUT => 0,
                Err(e) => return Err(ModemError::Io(format!("{:?}", e))),
            };
            if n == 0 {
                continue;
            }
            for event in self.parser.feed(&buf[..n]) {
                if let AtEvent::Line(line) = event {
                    let trimmed = line.trim();
                    log::info!("modem dial line: {:?}", trimmed);
                    if trimmed.starts_with("CONNECT") {
                        log::info!("modem: dial-up {}", trimmed);
                        return Ok(());
                    }
                    match classify(&line) {
                        LineClass::Error | LineClass::NoCarrier => {
                            log::warn!("modem: dial-up rejected: {}", trimmed);
                            return Err(ModemError::Error);
                        }
                        LineClass::CmeError(c) => {
                            log::warn!("modem: dial-up +CME ERROR {}", c);
                            return Err(ModemError::CmeError(c));
                        }
                        LineClass::CmsError(c) => return Err(ModemError::CmsError(c)),
                        _ => {}
                    }
                }
            }
        }
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

    fn query_reg_command(&mut self, cmd: &str, prefix: &str) -> RegistrationStatus {
        match self.send_raw(cmd) {
            Ok(lines) => {
                for line in &lines {
                    if let Some(rest) = line.trim().strip_prefix(prefix) {
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

    /// Set up the PDP context for the given APN and dial into data mode.
    /// Leaves the modem expecting PPP frames on the serial line.
    fn dial_data(&mut self, apn: &str) -> Result<(), ModemError> {
        // Verbose CME errors — turns "+CME ERROR: 11" into a real string
        // in the logs, much easier to debug. Ignore failure, it's a
        // nice-to-have.
        let _ = self.send_raw("AT+CMEE=2");
        // Explicit packet-domain attach. LTE modems usually auto-attach
        // once registered, but forcing it is harmless and rules out one
        // class of silent failure.
        let _ = self.send_raw("AT+CGATT=1");
        // Set the PDP context before dialing. EE's "everywhere" APN is
        // the carrier-provided IPv4 context.
        let cgdcont = format!("AT+CGDCONT=1,\"IP\",\"{}\"", apn);
        self.send_raw(&cgdcont)?;
        // Explicitly disable PAP/CHAP auth on context 1. Most A76xx
        // firmware versions default to "no auth" but some require this
        // to be set explicitly for PPP to succeed with a no-auth APN.
        let _ = self.send_raw("AT+CGAUTH=1,0");

        self.parser.reset();
        self.drain_uart();
        log::info!("modem: dialing ATD*99#");
        self.write_command("ATD*99#")?;
        self.wait_for_connect(Instant::now() + DIAL_TIMEOUT)
    }

    /// Create the PPP netif + driver and spawn the RX pump thread that
    /// owns them. Blocks until the thread signals that PPP has an IP
    /// address (via the `up_rx` channel) or the overall timeout fires.
    fn start_ppp_session(&mut self) -> Result<(), ModemError> {
        let netif = EspNetif::new(NetifStack::Ppp)
            .map_err(|e| ModemError::Io(format!("ppp netif new: {:?}", e)))?;

        // SAFETY: upgrade the Arc<UartDriver<'d>> to Arc<UartDriver<'static>>
        // so the tx closure and the rx pump thread can be 'static. The
        // UartDriver is held by `self.uart` which lives in main()'s
        // infinite event loop, and the thread is joined by
        // `disable_data` before the data session can be dropped. See
        // the module-level "Lifetime workaround" section.
        let uart_tx_static: Arc<UartDriver<'static>> =
            unsafe { std::mem::transmute(Arc::clone(&self.uart)) };
        let uart_rx_static: Arc<UartDriver<'static>> =
            unsafe { std::mem::transmute(Arc::clone(&self.uart)) };

        let mut netif_driver = EspNetifDriver::new(
            netif,
            |netif| {
                netif.set_ppp_conf(&PppConfiguration {
                    phase_events_enabled: false,
                    error_events_enabled: false,
                    ..Default::default()
                })
            },
            move |data| {
                uart_tx_static.write(data)?;
                Ok(())
            },
        )
        .map_err(|e| ModemError::Io(format!("ppp driver new: {:?}", e)))?;
        // Crucial: `EspNetifDriver::new` leaves the driver in `started:
        // false`. We have to call `start()` explicitly — under the hood
        // that calls `esp_netif_action_start`, which is what triggers
        // lwIP's PPP state machine to begin sending LCP configure
        // requests. Without this the driver sits there silently and
        // negotiation never starts, which was our 30-second "0 bytes"
        // timeout symptom.
        netif_driver
            .start()
            .map_err(|e| ModemError::Io(format!("ppp driver start: {:?}", e)))?;
        let send_driver = SendNetifDriver(netif_driver);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let (up_tx, up_rx) = mpsc::channel::<()>();

        let thread = thread::Builder::new()
            .name("ppp-rx-pump".into())
            .stack_size(4096)
            .spawn(move || ppp_rx_pump(send_driver, uart_rx_static, stop_for_thread, up_tx))
            .map_err(|e| ModemError::Io(format!("spawn ppp rx thread: {:?}", e)))?;

        // Wait for the thread to report PPP up. If it times out or the
        // channel closes (thread exited early), stop and join the thread
        // so we don't leak it, and surface the failure.
        let up_result = up_rx.recv_timeout(PPP_UP_TIMEOUT);

        let session = DataSession {
            stop,
            thread: Some(thread),
        };

        match up_result {
            Ok(()) => {
                log::info!("ppp up");
                self.data_session = Some(session);
                Ok(())
            }
            Err(e) => {
                let mut session = session;
                session.stop_and_join();
                Err(ModemError::Io(format!("ppp up timeout: {:?}", e)))
            }
        }
    }

    /// Send the `+++` escape sequence to return the modem from data mode
    /// to command mode. Per the A76xx datasheet the modem requires:
    ///   - at least 1 s of UART silence before the `+++`
    ///   - the three `+` characters with no other bytes interspersed
    ///   - at least 1 s of silence after, before it replies with `OK`
    /// We honour both guard times.
    fn escape_to_command_mode(&mut self) -> Result<(), ModemError> {
        let _ = self.uart.wait_tx_done(100);
        sleep(Duration::from_secs(1));
        self.uart
            .write(b"+++")
            .map_err(|e| ModemError::Io(format!("+++: {:?}", e)))?;
        sleep(Duration::from_secs(1));
        Ok(())
    }
}

impl Modem for EspA7682EModem<'_> {
    fn power_on(&mut self) -> Result<(), ModemError> {
        if self.powered {
            return Ok(());
        }
        log::info!("A7682E power-on pulse");
        self.pulse_pwrkey(Duration::from_millis(1000))?;

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
        // Tear down any active data session first so we don't leave PPP
        // dangling against a modem that's about to lose power.
        if self.data_session.is_some() {
            let _ = self.disable_data();
        }
        log::info!("A7682E power-off pulse");
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
        if self.data_session.is_some() {
            return Err(ModemError::DataActive);
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

    fn send_with_body(&mut self, cmd: &str, body: &[u8]) -> Result<Vec<String>, ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        if self.data_session.is_some() {
            return Err(ModemError::DataActive);
        }
        self.parser.reset();
        self.drain_uart();
        self.write_command(cmd)?;
        self.read_until_prompt(Instant::now() + PROMPT_TIMEOUT)?;
        self.uart
            .write(body)
            .map_err(|e| ModemError::Io(format!("{:?}", e)))?;
        self.read_until_final(Instant::now() + SEND_BODY_TIMEOUT)
    }

    fn enable_data(&mut self, apn: &str) -> Result<(), ModemError> {
        if !self.powered {
            return Err(ModemError::NotPowered);
        }
        if self.data_session.is_some() {
            return Ok(());
        }
        log::info!("enabling cellular data (APN={})", apn);

        // 1) PDP context + dial-up into data mode. After this the modem
        //    is expecting PPP frames on the UART, not AT commands.
        self.dial_data(apn)?;

        // 2) Create the PPP netif + driver, spawn the RX pump thread
        //    that owns them, and wait for PPP negotiation.
        self.start_ppp_session()
    }

    fn disable_data(&mut self) -> Result<(), ModemError> {
        let Some(mut session) = self.data_session.take() else {
            return Ok(());
        };
        log::info!("disabling cellular data");

        // Stop the RX pump thread. When it exits, its destructor drops
        // the EspNetifDriver it owns, which tears down the PPP stack and
        // closes the netif.
        session.stop_and_join();

        // The modem is still in data mode from its own perspective
        // (we haven't sent the escape sequence yet). Get it back to
        // command mode via `+++` with the A76xx datasheet guard times,
        // then `ATH` to hang up the data call.
        if let Err(e) = self.escape_to_command_mode() {
            log::warn!("escape to command mode failed: {:?}", e);
        }
        self.parser.reset();
        self.drain_uart();
        let _ = self.send_raw("ATH");
        Ok(())
    }

    fn is_data_active(&self) -> bool {
        self.data_session.is_some()
    }
}

/// Body of the RX pump thread spawned by `start_ppp_session`. Reads
/// UART bytes in a loop and feeds them into the PPP netif, exiting
/// when `stop` is set. Signals `up_tx` once — the first time the netif
/// reports `is_up() == true` — so `enable_data` can unblock.
///
/// The thread owns the `netif_driver` outright so we don't have to
/// share it with the main thread (which would require Sync that we
/// haven't proven). When the thread returns, the driver is dropped
/// here, which tears down PPP and closes the netif.
fn ppp_rx_pump(
    send_driver: SendNetifDriver,
    uart: Arc<UartDriver<'static>>,
    stop: Arc<AtomicBool>,
    up_tx: mpsc::Sender<()>,
) {
    let netif_driver = send_driver.0;
    let mut buf = [0u8; 512];
    let mut signalled_up = false;
    // Block up to ~50 ms per read so the stop flag is checked at least
    // that often even when the line is completely silent.
    const READ_TICKS_PUMP: u32 = 5;

    // Running totals for a one-line summary at shutdown. Intermediate
    // per-second log lines only fire when there's actual activity, to
    // keep the console quiet during idle cellular sessions.
    let mut total_bytes: u64 = 0;
    let mut bytes_this_sec: usize = 0;
    let mut last_tick = Instant::now();

    log::info!("ppp rx pump started");

    while !stop.load(Ordering::Relaxed) {
        match uart.read(&mut buf, READ_TICKS_PUMP) {
            Ok(n) if n > 0 => {
                bytes_this_sec += n;
                total_bytes += n as u64;
                if let Err(e) = netif_driver.rx(&buf[..n]) {
                    log::warn!("ppp rx ingest failed: {:?}", e);
                }
            }
            Ok(_) => {}
            Err(e) if e.code() == ESP_ERR_TIMEOUT => {}
            Err(e) => {
                log::warn!("ppp rx uart read error: {:?}", e);
                // Don't exit on a transient error — the modem will
                // recover once more bytes arrive.
            }
        }

        if last_tick.elapsed() >= Duration::from_secs(1) {
            if bytes_this_sec > 0 {
                log::info!("ppp rx: {} B/s", bytes_this_sec);
            }
            bytes_this_sec = 0;
            last_tick = Instant::now();
        }

        if !signalled_up {
            match netif_driver.netif().is_up() {
                Ok(true) => {
                    let _ = up_tx.send(());
                    signalled_up = true;
                }
                Ok(false) => {}
                Err(e) => log::warn!("ppp is_up check failed: {:?}", e),
            }
        }
    }
    log::info!("ppp rx pump stopped ({} bytes total)", total_bytes);
    // Dropping `netif_driver` here tears down PPP.
}
