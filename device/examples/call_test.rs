//! Make a real phone call via AT commands to validate the audio path end-to-end.
//!
//! This is the only true validation: record/playback tests go via the modem's
//! internal filesystem which introduces buffer artefacts unrelated to call
//! quality. VoLTE audio is a separate real-time codec path with AGC.
//!
//! Setup:
//!   - Plug in a TRRS headset (or a TRRS mic-only dongle) so the A7682E has
//!     a microphone input on the 3.5mm jack ring. The remote party will hear
//!     you through that mic. You will hear the remote party through the
//!     device's internal speaker (confirmed working from the TTS test).
//!   - Have a phone number to dial (a friend, voicemail, or a test line).
//!
//! Controls during the call:
//!   - u / d    → AT+COUTGAIN up / down (0-7, starts at default 4)
//!   - i / k    → AT+CLVL up / down (range unknown; check `q` output)
//!   - q        → log range + current value of COUTGAIN, CLVL, CSDVC, VMUTE
//!   - Enter    → hang up
//!
//! The status line shows duration and the current levels; the line below
//! shows whether the last command returned OK or ERR. Full responses are on
//! the serial monitor.
//!
//! Build:
//!   cargo build -p dynatac --example call_test
//! Flash:
//!   espflash flash target/xtensa-esp32s3-espidf/debug/examples/call_test --monitor

#[path = "../src/display.rs"]
mod display;
#[path = "../src/i2c_bus.rs"]
mod i2c_bus;
#[path = "../src/keyboard.rs"]
mod keyboard;
#[path = "../src/modem.rs"]
mod modem;

use std::thread::sleep;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, Dma, SpiConfig, SpiDeviceDriver, SpiDriver},
    uart::{config::Config as UartConfig, UartDriver},
    units::Hertz,
};

use display::Epd;
use dynatac_core::keymap::KeyEvent;
use dynatac_core::modem::{CallUrc, Modem};
use i2c_bus::I2cBus;
use keyboard::Keyboard;
use modem::EspA7682EModem;

const KEYBOARD_ADDR: u8 = 0x34;
const ROW_HEIGHT: u16 = 10;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    { let mut sd_cs = PinDriver::output(pins.gpio48).unwrap(); sd_cs.set_high().unwrap(); }

    let spi_driver = SpiDriver::new(
        peripherals.spi2, pins.gpio36, pins.gpio33, Some(pins.gpio47),
        &DriverConfig::new().dma(Dma::Auto(4096)),
    ).unwrap();
    let spi_device = SpiDeviceDriver::new(
        spi_driver, None::<AnyOutputPin>,
        &SpiConfig::new().baudrate(Hertz(10_000_000)),
    ).unwrap();
    let cs   = PinDriver::output(pins.gpio34.downgrade_output()).unwrap();
    let dc   = PinDriver::output(pins.gpio35.downgrade_output()).unwrap();
    let busy = PinDriver::input(pins.gpio37.downgrade()).unwrap();
    let mut epd = Epd::new(spi_device, cs, dc, busy);

    let i2c = I2cDriver::new(
        peripherals.i2c0, pins.gpio13, pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    ).unwrap();
    let i2c_bus = I2cBus::new(i2c);
    let mut kb = Keyboard::new(i2c_bus.device(KEYBOARD_ADDR)).unwrap();

    let modem_uart = UartDriver::new(
        peripherals.uart1, pins.gpio10, pins.gpio11,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &UartConfig::new().baudrate(Hertz(115_200)),
    ).unwrap();
    let mut modem = EspA7682EModem::new(
        modem_uart,
        PinDriver::output(pins.gpio40.downgrade_output()).unwrap(),
        PinDriver::output(pins.gpio41.downgrade_output()).unwrap(),
    );

    epd.clear().unwrap();
    let mut y = 0u16;

    // --- Boot + modem power-on ---------------------------------------------------
    print_line(&mut epd, "=== Call Test ===", &mut y);
    print_line(&mut epd, "Powering on modem...", &mut y);
    match modem.power_on() {
        Ok(()) => print_line(&mut epd, "[OK] Modem ready.", &mut y),
        Err(e) => {
            print_line(&mut epd, &format!("[ERR] {:?}", e), &mut y);
            loop { sleep(Duration::from_secs(1)); }
        }
    }

    // --- Number entry ------------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Enter number, then", &mut y);
    print_line(&mut epd, "press Enter to dial.", &mut y);
    print_line(&mut epd, "(backspace to delete)", &mut y);
    print_line(&mut epd, "", &mut y);

    let number = enter_number(&mut epd, &mut kb, &mut y);
    if number.is_empty() {
        print_line(&mut epd, "No number. Exiting.", &mut y);
        sleep(Duration::from_secs(2));
        epd.power_down().unwrap();
        return;
    }

    // --- Dial --------------------------------------------------------------------
    // AT+CVHU=0: make ATH terminate voice calls (default on some firmware is
    // AT+CVHU=1 which ignores ATH for voice). AT+CHUP is used for hang-up but
    // this ensures ATH also works as a fallback.
    let _ = modem.send_raw("AT+CVHU=0");

    let dial_cmd = format!("ATD{};", number); // semicolon = voice call
    log::info!("Dialling: {}", dial_cmd);
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, &format!("Dialling {}...", number), &mut y);

    match modem.send_raw(&dial_cmd) {
        Ok(_) => {
            print_line(&mut epd, "Ringing...", &mut y);
            print_line(&mut epd, "u/d: COUTGAIN up/down", &mut y);
            print_line(&mut epd, "i/k: CLVL up/down", &mut y);
            print_line(&mut epd, "q: query (see serial)", &mut y);
            print_line(&mut epd, "Enter: hang up", &mut y);
        }
        Err(e) => {
            log::error!("ATD failed: {:?}", e);
            print_line(&mut epd, &format!("[ERR] {:?}", e), &mut y);
            print_line(&mut epd, "Press any key.", &mut y);
            wait_key(&mut kb);
            epd.power_down().unwrap();
            return;
        }
    }

    // --- In-call loop ------------------------------------------------------------
    // Poll the keyboard for hang-up, and poll the modem UART for URCs
    // (NO CARRIER, BUSY, etc.) to detect if the call ends remotely.
    //
    // Volume experiment: the earpiece level was too quiet in earlier calls.
    // AT+COUTGAIN is 0-7 with 4 the default (LilyGo test_AT README), so we
    // step it by keypress and watch the result. AT+CLVL may only be accepted
    // once the remote answers, so its range is unknown until queried with `q`.
    let call_start = Instant::now();
    let mut last_display_update = Instant::now();
    let status_y = y;
    let result_y = y + ROW_HEIGHT;
    y += 2 * ROW_HEIGHT;
    let mut out_gain: u8 = 4;
    let mut clvl: Option<u8> = None;
    let mut last_result = String::from("q after answer");
    // Run the queries once automatically 3 s in, in case the call is up by then.
    let mut auto_queried = false;
    let auto_query_at = call_start + Duration::from_secs(3);

    loop {
        let key = match kb.poll() {
            Ok(Some(k)) => Some(k),
            Ok(None) | Err(_) => None,
        };
        let mut force_redraw = false;
        match key {
            Some(KeyEvent::Enter) => {
                log::info!("User pressed Enter — hanging up");
                break;
            }
            Some(KeyEvent::Char('u')) if out_gain < 7 => {
                out_gain += 1;
                last_result = set_level(&mut modem, "COUTGAIN", out_gain);
                force_redraw = true;
            }
            Some(KeyEvent::Char('d')) if out_gain > 0 => {
                out_gain -= 1;
                last_result = set_level(&mut modem, "COUTGAIN", out_gain);
                force_redraw = true;
            }
            Some(KeyEvent::Char('i')) => {
                let next = clvl.map_or(0, |v| v.saturating_add(1));
                last_result = set_level(&mut modem, "CLVL", next);
                if last_result.ends_with("OK") {
                    clvl = Some(next);
                }
                force_redraw = true;
            }
            Some(KeyEvent::Char('k')) => {
                let next = clvl.map_or(0, |v| v.saturating_sub(1));
                last_result = set_level(&mut modem, "CLVL", next);
                if last_result.ends_with("OK") {
                    clvl = Some(next);
                }
                force_redraw = true;
            }
            Some(KeyEvent::Char('q')) => {
                clvl = query_audio(&mut modem).or(clvl);
                last_result = format!("queried, CLVL={}", fmt_level(clvl));
                force_redraw = true;
            }
            _ => {}
        }

        if !auto_queried && Instant::now() >= auto_query_at {
            auto_queried = true;
            clvl = query_audio(&mut modem).or(clvl);
        }

        // Poll modem for URCs (NO CARRIER, BUSY, etc.)
        if let Some(event) = modem.poll_urc() {
            log::info!("URC during call: {:?}", event);
            match event {
                CallUrc::NoCarrier => {
                    print_line(&mut epd, "", &mut y);
                    print_line(&mut epd, "Call ended.", &mut y);
                    print_line(&mut epd, "Press any key.", &mut y);
                    wait_key(&mut kb);
                    epd.power_down().unwrap();
                    return;
                }
                CallUrc::Busy => {
                    print_line(&mut epd, "", &mut y);
                    print_line(&mut epd, "Busy.", &mut y);
                    print_line(&mut epd, "Press any key.", &mut y);
                    wait_key(&mut kb);
                    epd.power_down().unwrap();
                    return;
                }
                CallUrc::Other(s) => {
                    log::info!("other URC: {}", s);
                }
            }
        }

        // Status line (duration + levels) and last command result, redrawn
        // in place every second or immediately after a keypress.
        if force_redraw || last_display_update.elapsed() >= Duration::from_secs(1) {
            last_display_update = Instant::now();
            let secs = call_start.elapsed().as_secs();
            let status = format!(
                "{:02}:{:02} gain:{} clvl:{}",
                secs / 60,
                secs % 60,
                out_gain,
                fmt_level(clvl)
            );
            draw_line_at(&mut epd, &status, status_y);
            draw_line_at(&mut epd, &last_result, result_y);
            let _ = epd.try_flush();
        }

        sleep(Duration::from_millis(50));
    }

    // --- Hang up -----------------------------------------------------------------
    // AT+CHUP is the 3GPP voice-call hang-up command. ATH is also sent as a
    // fallback (effective when AT+CVHU=0 is set, which we did before dialling).
    let r = modem.send_raw("AT+CHUP");
    log::info!("AT+CHUP: {:?}", r);
    let r = modem.send_raw("ATH");
    log::info!("ATH: {:?}", r);
    let secs = call_start.elapsed().as_secs();
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Call ended.", &mut y);
    print_line(&mut epd, &format!("Duration {:02}:{:02}", secs / 60, secs % 60), &mut y);
    print_line(&mut epd, "Press any key.", &mut y);
    wait_key(&mut kb);

    epd.power_down().unwrap();
}

// ---------------------------------------------------------------------------
// Number entry with keyboard
// ---------------------------------------------------------------------------

fn enter_number(epd: &mut Epd, kb: &mut Keyboard, y: &mut u16) -> String {
    let mut number = String::new();
    redraw_number(epd, &number, y);

    loop {
        match kb.poll() {
            Ok(Some(KeyEvent::Char(c))) => {
                if c.is_ascii_digit() || c == '+' {
                    number.push(c);
                    redraw_number(epd, &number, y);
                }
            }
            Ok(Some(KeyEvent::Backspace)) => {
                number.pop();
                redraw_number(epd, &number, y);
            }
            Ok(Some(KeyEvent::Enter)) => return number,
            Ok(_) | Err(_) => {}
        }
        sleep(Duration::from_millis(10));
    }
}

fn redraw_number(epd: &mut Epd, number: &str, y: &mut u16) {
    epd.clear_line(*y);
    let display = format!("> {}_", number);
    for (i, ch) in display.chars().take(30).enumerate() {
        epd.draw_char((i * 8) as u8, *y, ch).unwrap();
    }
    let _ = epd.try_flush();
}

// ---------------------------------------------------------------------------
// Volume experiment
// ---------------------------------------------------------------------------

/// Send `AT+<cmd>=<level>` with a short timeout so the call loop stays
/// responsive. Returns a short on-screen summary ending in "OK" or "ERR".
fn set_level(modem: &mut EspA7682EModem, cmd: &str, level: u8) -> String {
    let at = format!("AT+{}={}", cmd, level);
    let r = modem.send_raw_with_timeout(&at, Duration::from_secs(1));
    log::info!("{}: {:?}", at, r);
    match r {
        Ok(_) => format!("{}={} OK", cmd, level),
        Err(_) => format!("{}={} ERR", cmd, level),
    }
}

/// Log the supported range and current value of every audio-output command
/// we know of. Returns the current CLVL if the modem reported one.
fn query_audio(modem: &mut EspA7682EModem) -> Option<u8> {
    let mut clvl = None;
    for at in [
        "AT+COUTGAIN=?", "AT+COUTGAIN?",
        "AT+CLVL=?", "AT+CLVL?",
        "AT+CSDVC=?", "AT+CSDVC?",
        "AT+VMUTE?",
    ] {
        let r = modem.send_raw_with_timeout(at, Duration::from_secs(1));
        log::info!("{}: {:?}", at, r);
        if at == "AT+CLVL?" {
            clvl = r.ok().and_then(|lines| parse_level(&lines, "+CLVL:"));
        }
    }
    clvl
}

/// Parse the integer from an info line such as `+CLVL: 3`.
fn parse_level(lines: &[String], prefix: &str) -> Option<u8> {
    lines
        .iter()
        .find_map(|l| l.trim().strip_prefix(prefix))
        .and_then(|rest| rest.trim().parse().ok())
}

fn fmt_level(level: Option<u8>) -> String {
    level.map_or_else(|| "?".to_string(), |v| v.to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Overwrite a single row in place (no flush).
fn draw_line_at(epd: &mut Epd, text: &str, y: u16) {
    if y >= display::HEIGHT { return; }
    epd.clear_line(y);
    for (i, ch) in text.chars().take(30).enumerate() {
        epd.draw_char((i * 8) as u8, y, ch).unwrap();
    }
}

fn print_line(epd: &mut Epd, text: &str, y: &mut u16) {
    if *y >= display::HEIGHT { return; }
    epd.clear_line(*y);
    for (i, ch) in text.chars().take(30).enumerate() {
        epd.draw_char((i * 8) as u8, *y, ch).unwrap();
    }
    let _ = epd.try_flush();
    *y += ROW_HEIGHT;
}

fn wait_key(kb: &mut Keyboard) {
    loop {
        match kb.poll() {
            Ok(Some(_)) => return,
            Ok(None) | Err(_) => {}
        }
        sleep(Duration::from_millis(10));
    }
}
