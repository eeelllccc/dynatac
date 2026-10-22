//! Manual integration test: keyboard → e-paper terminal.
//!
//! Displays a cursor at the bottom-left of the screen. Typed characters appear
//! left-to-right.  Enter clears the line.  Backspace removes the last character.
//!
//! Build:  cargo build -p dynatac --example keyboard_terminal
//! Flash:  espflash flash target/xtensa-esp32s3-espidf/debug/examples/keyboard_terminal --monitor

#[path = "../src/display.rs"]
mod display;
#[path = "../src/i2c_bus.rs"]
mod i2c_bus;
#[path = "../src/keyboard.rs"]
mod keyboard;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, SpiConfig, SpiDeviceDriver, SpiDriver},
    units::Hertz,
};

use display::Epd;
use i2c_bus::I2cBus;
use dynatac_core::font::FontSize;
use dynatac_core::keymap::KeyEvent;
use keyboard::Keyboard;

const FONT: FontSize = FontSize::Medium;

/// Max characters per line.
const LINE_COLS: usize = (display::WIDTH / FONT.width()) as usize;

/// The text line sits at the very bottom of the 320-pixel-tall display.
const LINE_Y: u16 = display::HEIGHT - FONT.height();

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting keyboard terminal example");

    let peripherals = Peripherals::take().unwrap();
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

    // --- I2C bus for keyboard ----------------------------------------------------
    let i2c = I2cDriver::new(
        peripherals.i2c0,
        pins.gpio13,
        pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    )
    .unwrap();

    let i2c_bus = I2cBus::new(i2c);
    let mut kb = Keyboard::new(i2c_bus.device(0x34)).unwrap();

    // --- Init display ------------------------------------------------------------
    log::info!("Clearing display");
    epd.clear().unwrap();

    // Draw initial cursor: underscore at (0, LINE_Y)
    let mut buf = ['\0'; LINE_COLS];
    let mut cursor: usize = 0;
    redraw_line(&mut epd, &buf, cursor);

    // --- Event loop --------------------------------------------------------------
    log::info!("Ready — type on the keyboard");
    loop {
        // 1. Drain all buffered key events (fast — just updates buf/cursor).
        let dirty = drain_keys(&mut kb, &mut buf, &mut cursor);

        // 2. Only redraw + flush if something changed.
        if dirty {
            redraw_line(&mut epd, &buf, cursor);
            log::info!("flushing to screen...");
            if let Err(e) = epd.try_flush() {
                log::error!("display flush error: {:?}", e);
            }
            log::info!("flush complete");
        }

        sleep(Duration::from_millis(10));
    }
}

/// Drain all pending key events from the TCA8418 FIFO into the line buffer.
/// Returns true if any events were processed.
fn drain_keys(kb: &mut Keyboard, buf: &mut [char; LINE_COLS], cursor: &mut usize) -> bool {
    let mut got_key = false;
    loop {
        match kb.poll() {
            Ok(Some(event)) => {
                got_key = true;
                match event {
                    KeyEvent::Enter => {
                        *buf = ['\0'; LINE_COLS];
                        *cursor = 0;
                        log::info!("key: [enter]");
                    }
                    KeyEvent::Backspace => {
                        if *cursor > 0 {
                            *cursor -= 1;
                            buf[*cursor] = '\0';
                            log::info!("key: [backspace] cursor={}", cursor);
                        }
                    }
                    KeyEvent::Char(ch) => {
                        if *cursor < LINE_COLS {
                            buf[*cursor] = ch;
                            *cursor += 1;
                            log::info!("key: '{}' cursor={}", ch, cursor);
                        }
                    }
                    other => log::info!("key: {:?} (ignored)", other),
                }
            }
            Ok(None) => break,
            Err(e) => {
                log::error!("keyboard poll error: {:?}", e);
                break;
            }
        }
    }
    got_key
}

/// Redraw the bottom line into the desired buffer (no display I/O).
fn redraw_line(epd: &mut Epd, buf: &[char; LINE_COLS], cursor: usize) {
    epd.clear_line(LINE_Y, FONT.height());

    for (i, &ch) in buf.iter().enumerate() {
        if ch == '\0' {
            break;
        }
        let x = i as u16 * FONT.width();
        epd.draw_glyph(x, LINE_Y, FONT.font(), ch);
    }

    if cursor < LINE_COLS {
        let x = cursor as u16 * FONT.width();
        epd.draw_glyph(x, LINE_Y, FONT.font(), '_');
    }
}
