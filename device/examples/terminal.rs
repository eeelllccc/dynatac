//! Manual integration test: keyboard → Terminal module → e-paper display.
//!
//! Wires the Terminal module from dynatac-core to the real hardware.
//! Type commands and press Enter. For now, commands just echo back.
//!
//! Build:  cargo build -p dynatac --example terminal
//! Flash:  espflash flash target/xtensa-esp32s3-espidf/debug/examples/terminal --monitor

#[path = "../src/display.rs"]
mod display;
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
use dynatac_core::terminal::{Terminal, TerminalAction};
use keyboard::Keyboard;

/// Row height in pixels: 8px font + 2px gap.
const ROW_HEIGHT: u16 = 10;
/// Number of text columns (240px / 8px per glyph).
const TERM_COLS: usize = (display::WIDTH as usize) / 8;
/// Number of text rows (320px / 10px per row).
const TERM_ROWS: usize = (display::HEIGHT as usize) / ROW_HEIGHT as usize;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting terminal example");

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

    let mut kb = Keyboard::new(i2c).unwrap();

    // --- Init display + terminal -------------------------------------------------
    log::info!("Clearing display");
    epd.clear().unwrap();

    let mut term = Terminal::new("> ", TERM_COLS, TERM_ROWS);

    // Initial render: draw the prompt
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
                    match term.handle_key(event) {
                        TerminalAction::Redraw => {
                            needs_redraw = true;
                        }
                        TerminalAction::Execute(cmd) => {
                            // Simple command handler: echo the command back
                            let output = execute(&cmd);
                            term.push_output(&output);
                            needs_redraw = true;
                        }
                        TerminalAction::None => {}
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

/// Render the full terminal state to the EPD framebuffer.
fn render_terminal(epd: &mut Epd, term: &Terminal) {
    // Clear all pixel rows used by the terminal (each text row = ROW_HEIGHT px)
    for row in 0..TERM_ROWS {
        epd.clear_line((row as u16) * ROW_HEIGHT);
    }

    // Draw each cell from the terminal
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

/// Toy command handler. Returns the output string for a given command.
fn execute(cmd: &str) -> String {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return String::new();
    }
    match cmd {
        "help" => "commands: help, hello, echo <msg>".to_string(),
        "hello" => "Hello from dynatac!".to_string(),
        _ if cmd.starts_with("echo ") => cmd[5..].to_string(),
        _ => format!("unknown command: {}", cmd),
    }
}
