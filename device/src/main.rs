mod battery;
mod charger;
mod display;
mod http;
mod i2c_bus;
pub mod keyboard;
mod modem;
mod nvs_credential_store;
mod nvs_network_store;
mod sleep;
mod smtp;
mod wifi;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver, Pull},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, SpiConfig, SpiDeviceDriver, SpiDriver},
    uart::{config::Config as UartConfig, UartDriver},
    units::Hertz,
};

use battery::Battery;
use charger::Charger;
use display::Epd;
use dynatac_core::battery::BQ27220_ADDR;
use dynatac_core::charger::{ChargerDriver, BQ25896_ADDR};
use dynatac_core::keymap::KeyEvent;
use dynatac_core::modem::Modem;
use dynatac_core::power::{Power, PowerAction};
use dynatac_core::programs::ExecContext;
use dynatac_core::shell::{Shell, ShellAction};
use dynatac_core::terminal::{Terminal, TerminalAction};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use http::EspHttpClient;
use i2c_bus::I2cBus;
use modem::EspA7682EModem;
use nvs_credential_store::NvsCredentialStore;
use nvs_network_store::NvsNetworkStore;
use smtp::EspSmtpStreamFactory;
use wifi::EspWifiDriver;
use keyboard::Keyboard;

/// TCA8418 keyboard I2C address.
const KEYBOARD_ADDR: u8 = 0x34;

/// Row height in pixels: 8px font + 2px gap.
const ROW_HEIGHT: u16 = 10;
/// Number of text columns (240px / 8px per glyph).
const TERM_COLS: usize = (display::WIDTH as usize) / 8;
/// Number of text rows (320px / 10px per row).
const TERM_ROWS: usize = (display::HEIGHT as usize) / ROW_HEIGHT as usize;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting dynatac OS");

    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let pins = peripherals.pins;

    // --- Deselect other SPI devices on the shared bus ----------------------------
    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
    sd_cs.set_high().unwrap();

    // --- SPI bus for EPD ---------------------------------------------------------
    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        pins.gpio36,
        pins.gpio33,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &DriverConfig::new(),
    )
    .unwrap();

    let spi_device = SpiDeviceDriver::new(
        spi_driver,
        None::<AnyOutputPin>,
        &SpiConfig::new().baudrate(Hertz(10_000_000)),
    )
    .unwrap();

    let cs = PinDriver::output(pins.gpio34.downgrade_output()).unwrap();
    let dc = PinDriver::output(pins.gpio35.downgrade_output()).unwrap();
    let busy = PinDriver::input(pins.gpio37.downgrade()).unwrap();

    let mut epd = Epd::new(spi_device, cs, dc, busy);

    // --- Shared I2C bus (keyboard, touch, gyro, fuel gauge, charger) ------------
    let i2c_driver = I2cDriver::new(
        peripherals.i2c0,
        pins.gpio13,
        pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    )
    .unwrap();
    let i2c_bus = I2cBus::new(i2c_driver);

    let mut kb = Keyboard::new(i2c_bus.device(KEYBOARD_ADDR)).unwrap();
    let mut battery = Battery::new(i2c_bus.device(BQ27220_ADDR));
    let mut charger = Charger::new(i2c_bus.device(BQ25896_ADDR));

    // Scan the I2C bus for responsive devices. This is a one-time
    // diagnostic to confirm which addresses are alive — the BQ25896
    // might be at 0x6B (datasheet) or 0x6A (SY6970 variant).
    {
        let mut found = Vec::new();
        for addr in 0x03..=0x77u8 {
            let dev = i2c_bus.device(addr);
            if dev.write(&[0x00]).is_ok() {
                found.push(addr);
            }
        }
        log::info!(
            "I2C bus scan: {} device(s) found: {}",
            found.len(),
            found.iter().map(|a| format!("0x{:02X}", a)).collect::<Vec<_>>().join(", ")
        );
    }

    // Disable the BQ25896 I2C watchdog FIRST. With the watchdog
    // running, the chip reverts all our REG09 customisations to
    // defaults every ~40 s, silently re-enabling BATFET_RST_EN.
    log::info!("configuring BQ25896 at 0x{:02X}...", BQ25896_ADDR);
    if let Err(e) = charger.disable_watchdog() {
        log::warn!("could not disable BQ25896 watchdog: {:?}", e);
    } else {
        log::info!("BQ25896 watchdog disabled");
    }

    // Now clear BATFET_RST_EN so holding the power button while the
    // device is running no longer triggers a PMIC-level reset.
    if let Err(e) = charger.disable_long_press_reset() {
        log::warn!("could not disable BQ25896 long-press reset: {:?}", e);
    } else {
        log::info!("BQ25896 long-press reset disabled");
    }

    // Own GPIO15 (TCA8418 IRQ, active LOW open-drain) as a pulled-up
    // input. Used both for ext0 light-sleep wake and as a foundation
    // if we later want interrupt-driven keyboard reads. Held for the
    // lifetime of `main`.
    let mut kb_irq = PinDriver::input(pins.gpio15.downgrade()).unwrap();
    kb_irq.set_pull(Pull::Up).unwrap();

    // --- Drivers -----------------------------------------------------------------
    let mut wifi = EspWifiDriver::new(peripherals.modem, sysloop, Some(nvs.clone()));
    let mut http_client = EspHttpClient::new();
    let mut saved_networks = NvsNetworkStore::new(nvs.clone());
    let mut credentials = NvsCredentialStore::new(nvs);
    let mut smtp_factory = EspSmtpStreamFactory::new();

    // --- A7682E 4G modem (off by default; powered on by `modem on`) ---------
    // Board pin map (from T-Deck-Pro utilities.h):
    //   POWER_EN = GPIO41, PWRKEY = GPIO40,
    //   modem RX = GPIO10 (ESP TX), modem TX = GPIO11 (ESP RX).
    // UART1 is free (UART0 is the USB console, the main ESP wifi/BT peripheral
    // used elsewhere is unrelated to the hardware UARTs).
    let uart_config = UartConfig::new().baudrate(Hertz(115_200));
    let modem_uart = UartDriver::new(
        peripherals.uart1,
        pins.gpio10,
        pins.gpio11,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &uart_config,
    )
    .unwrap();
    let modem_pwrkey = PinDriver::output(pins.gpio40.downgrade_output()).unwrap();
    let modem_power_en = PinDriver::output(pins.gpio41.downgrade_output()).unwrap();
    let mut modem_driver = EspA7682EModem::new(modem_uart, modem_pwrkey, modem_power_en);

    // --- Init display + terminal + shell -----------------------------------------
    log::info!("Clearing display");
    epd.clear().unwrap();

    let mut term = Terminal::new("> ", TERM_COLS, TERM_ROWS);
    let mut shell = Shell::new();
    shell.set_display_rows(TERM_ROWS);
    let mut power = Power::new();
    let boot = std::time::Instant::now();

    // --- Startup tasks ----------------------------------------------------------
    let boot_log = dynatac_core::startup::run_startup(&mut wifi, &mut saved_networks);
    for msg in &boot_log {
        term.push_output(msg);
    }

    // Initial render: draw the prompt + boot messages
    render_terminal(&mut epd, &term);
    flush(&mut epd);

    // --- Event loop --------------------------------------------------------------
    log::info!("Ready — type on the keyboard");
    loop {
        let mut needs_redraw = false;

        // Drain all buffered key events
        loop {
            match kb.poll() {
                Ok(Some(event)) => {
                    // Route every key through the power state machine
                    // first. Alt+L is the only key that can lock, and
                    // (once locked) the only key that can unlock.
                    match power.handle_key(event) {
                        PowerAction::EnterLock => {
                            log::info!("locking");
                            handle_lock(
                                &mut epd,
                                &mut wifi,
                                &mut modem_driver,
                                &mut kb,
                                &mut power,
                            );
                            log::info!("unlocking");
                            handle_unlock(&mut epd, &term);
                            needs_redraw = false;
                            // The Alt+L sequence is fully consumed by
                            // the lock/unlock cycle; do NOT pass it
                            // through to the terminal.
                            continue;
                        }
                        // ExitLock is impossible here — we never reach
                        // this loop while locked, because handle_lock
                        // owns the entire locked-state lifecycle.
                        PowerAction::ExitLock | PowerAction::None => {}
                    }

                    if shell.is_interactive() {
                        // Route keys to the active list selector
                        let mut ctx = ExecContext {
                            uptime_secs: boot.elapsed().as_secs(),
                            wifi: &mut wifi,
                            http: &mut http_client,
                            saved_networks: &mut saved_networks,
                            smtp: &mut smtp_factory,
                            credentials: &mut credentials,
                            modem: &mut modem_driver,
                            battery: &mut battery,
                            charger: &mut charger,
                        };
                        let was_interactive = true;
                        match shell.handle_interactive_key(event, &mut ctx) {
                            ShellAction::Output(output) => {
                                if !output.is_empty() {
                                    term.clear();
                                    term.push_output(&output);
                                }
                            }
                            ShellAction::Clear => {
                                term.clear();
                            }
                            ShellAction::PowerOff => {
                                handle_power_off(
                                    &mut epd,
                                    &mut term,
                                    &mut wifi,
                                    &mut modem_driver,
                                );
                            }
                            ShellAction::ShipMode => {
                                handle_ship_mode(
                                    &mut epd,
                                    &mut term,
                                    &mut wifi,
                                    &mut modem_driver,
                                    &mut charger,
                                );
                            }
                        }
                        // Exited list mode — restore input line
                        if was_interactive && !shell.is_interactive() {
                            term.set_show_input(true);
                        }
                        needs_redraw = true;
                    } else {
                        // Normal terminal input
                        match term.handle_key(event) {
                            TerminalAction::Redraw => {
                                needs_redraw = true;
                            }
                            TerminalAction::Execute(cmd) => {
                                let mut ctx = ExecContext {
                                    uptime_secs: boot.elapsed().as_secs(),
                                    wifi: &mut wifi,
                                    http: &mut http_client,
                                    saved_networks: &mut saved_networks,
                                    smtp: &mut smtp_factory,
                                    credentials: &mut credentials,
                                    modem: &mut modem_driver,
                                    battery: &mut battery,
                                    charger: &mut charger,
                                };
                                match shell.execute(&cmd, &mut ctx) {
                                    ShellAction::Output(output) => {
                                        // Entered list mode — hide input line
                                        if shell.is_interactive() {
                                            term.set_show_input(false);
                                            term.clear();
                                        }
                                        term.push_output(&output);
                                    }
                                    ShellAction::Clear => {
                                        term.clear();
                                    }
                                    ShellAction::PowerOff => {
                                        term.set_show_input(false);
                                        term.clear();
                                        handle_power_off(
                                            &mut epd,
                                            &mut term,
                                            &mut wifi,
                                            &mut modem_driver,
                                        );
                                    }
                                    ShellAction::ShipMode => {
                                        term.set_show_input(false);
                                        term.clear();
                                        handle_ship_mode(
                                            &mut epd,
                                            &mut term,
                                            &mut wifi,
                                            &mut modem_driver,
                                            &mut charger,
                                        );
                                    }
                                }
                                needs_redraw = true;
                            }
                            TerminalAction::None => {}
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::error!("keyboard poll error: {:?}", e);
                    break;
                }
            }
        }

        if needs_redraw {
            render_terminal(&mut epd, &term);
            flush(&mut epd);
        }

        sleep(Duration::from_millis(10));
    }
}

/// Owns the entire locked-state lifecycle. Paints the lockscreen,
/// powers down peripherals, and then loops:
///
///   1. Drain the keyboard FIFO. If the user pressed Alt+L, exit.
///   2. Otherwise enter light sleep until the next key press.
///
/// Only Alt+L (`KeyEvent::Lock`) unlocks. Every other key event is
/// silently consumed and the MCU goes back to sleep, so a stray key
/// press doesn't surface a stale character to the terminal on resume.
///
/// On exit the [`Power`] state machine is flipped back to Active so
/// the caller's view of state matches reality.
fn handle_lock(
    epd: &mut Epd,
    wifi: &mut EspWifiDriver,
    modem_driver: &mut EspA7682EModem,
    kb: &mut Keyboard,
    power: &mut Power,
) {
    if let Err(e) = epd.present_lockscreen() {
        log::error!("lockscreen render failed: {:?}", e);
    }
    if let Err(e) = epd.power_down() {
        log::warn!("EPD power_down failed: {:?}", e);
    }
    // Best-effort radio shutdown. Ignore errors — we still want to sleep.
    if let Err(e) = wifi.shutdown_for_sleep() {
        log::warn!("wifi shutdown failed: {:?}", e);
    }
    if let Err(e) = modem_driver.power_off() {
        log::warn!("modem power_off failed: {:?}", e);
    }

    loop {
        // Drain the FIFO. The TCA8418 keeps the IRQ line asserted while
        // it has events queued, so this both handles a wake we just got
        // and clears any backlog before sleeping again.
        let mut got_unlock = false;
        loop {
            match kb.poll() {
                Ok(Some(KeyEvent::Lock)) => got_unlock = true,
                Ok(Some(_)) => {
                    // Discard — only Alt+L unlocks.
                }
                Ok(None) => break,
                Err(e) => {
                    log::warn!("kb poll while locked: {:?}", e);
                    break;
                }
            }
        }
        if got_unlock {
            // Wipe any Shift / Sym / Alt toggles the user accidentally
            // pressed while locked. The Alt+L sequence itself
            // auto-cleared Alt; this catches Shift / Sym leakage.
            kb.clear_modifiers();
            // Re-sync the state machine. The actual transition was
            // taken implicitly by this function returning.
            let _ = power.handle_key(KeyEvent::Lock);
            return;
        }

        if let Err(e) = sleep::enter() {
            log::warn!("light sleep entry failed: {:?}", e);
            // If light sleep is rejected (e.g. wake source already
            // pending) we just loop and re-drain the FIFO.
        }
    }
}

/// Multi-step power-off sequence using ESP32 deep sleep.
///
/// After shutting down radios and blanking the display, the MCU enters
/// deep sleep with no configured wake source. The physical power button
/// is wired to CHIP_PU (the hardware reset line); pressing it pulls
/// CHIP_PU low, which resets the chip. When the button is released the
/// chip boots fresh from ROM — the user sees a clean power-on.
///
/// This function never returns.
fn handle_power_off(
    epd: &mut Epd,
    term: &mut Terminal,
    wifi: &mut EspWifiDriver,
    modem_driver: &mut EspA7682EModem,
) -> ! {
    log::info!("powering off (deep sleep)");
    let dwell = Duration::from_millis(600);

    term.push_output("powering off…");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);

    term.push_output("  stopping wifi");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);
    if let Err(e) = wifi.shutdown_for_sleep() {
        log::warn!("wifi shutdown failed: {:?}", e);
    }

    term.push_output("  stopping modem");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);
    if let Err(e) = modem_driver.power_off() {
        log::warn!("modem power_off failed: {:?}", e);
    }

    term.push_output("  press button to wake");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);

    if let Err(e) = epd.clear() {
        log::warn!("EPD clear during power-off failed: {:?}", e);
    }
    if let Err(e) = epd.power_down() {
        log::warn!("EPD power_down during power-off failed: {:?}", e);
    }

    sleep::power_off()
}

/// Multi-step BQ25896 ship-mode sequence.
///
/// Ship mode disconnects the battery from the system. The only way to
/// restore power is to connect USB. Use this for long-term storage or
/// shipping — not for everyday "power off" use.
///
/// This function never returns: on battery the BQ25896 yanks power
/// within tens of milliseconds; on USB the chip ignores the request
/// and we spin with a blank screen (which is fine — the user is plugged in).
fn handle_ship_mode(
    epd: &mut Epd,
    term: &mut Terminal,
    wifi: &mut EspWifiDriver,
    modem_driver: &mut EspA7682EModem,
    charger: &mut Charger,
) -> ! {
    log::info!("entering ship mode");
    let dwell = Duration::from_millis(600);

    term.push_output("entering ship mode…");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);

    term.push_output("  stopping wifi");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);
    if let Err(e) = wifi.shutdown_for_sleep() {
        log::warn!("wifi shutdown failed: {:?}", e);
    }

    term.push_output("  stopping modem");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);
    if let Err(e) = modem_driver.power_off() {
        log::warn!("modem power_off failed: {:?}", e);
    }

    term.push_output("  disconnecting battery");
    render_terminal(epd, term);
    flush(epd);
    sleep(dwell);

    if let Err(e) = epd.clear() {
        log::warn!("EPD clear during ship-mode failed: {:?}", e);
    }
    if let Err(e) = epd.power_down() {
        log::warn!("EPD power_down during ship-mode failed: {:?}", e);
    }

    if let Err(e) = charger.shutdown() {
        log::warn!("charger shutdown failed: {:?}", e);
    }

    loop {
        sleep(Duration::from_secs(1));
    }
}

/// Wake-from-lock: bring the panel back up and re-render the terminal.
/// The radios stay off — the user re-enables them explicitly.
fn handle_unlock(epd: &mut Epd, term: &Terminal) {
    if let Err(e) = epd.power_on() {
        log::warn!("EPD power_on after unlock failed: {:?}", e);
    }
    // The framebuffer still holds the lockscreen image; mark the whole
    // screen dirty by clearing it back to the terminal contents and
    // re-rendering everything. We do this via the same full-screen
    // path used for the lockscreen so the next partial flushes line up.
    if let Err(e) = epd.clear() {
        log::warn!("EPD clear after unlock failed: {:?}", e);
    }
    render_terminal(epd, term);
    flush(epd);
}

/// Render the full terminal state to the EPD framebuffer.
fn render_terminal(epd: &mut Epd, term: &Terminal) {
    for row in 0..TERM_ROWS {
        epd.clear_line((row as u16) * ROW_HEIGHT);
    }
    for cell in term.render() {
        let x = cell.col * 8;
        let y = cell.row as u16 * ROW_HEIGHT;
        epd.draw_char(x, y, cell.ch).unwrap();
    }
}

/// Flush dirty regions to the e-paper display.
fn flush(epd: &mut Epd) {
    if let Err(e) = epd.try_flush() {
        log::error!("display flush error: {:?}", e);
    }
}
