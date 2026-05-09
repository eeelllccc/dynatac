//! Interactive demo for the scrollable ListSelector.
//!
//! Boot shows an intro screen. Press Enter to open a list of 50 items.
//! Navigate with y (up) / h (down), confirm with Enter.
//! The selected item is shown on the intro screen; press Enter to go again.
//!
//! Build:  cargo build -p dynatac --example list_selector_demo
//! Flash:  espflash flash target/xtensa-esp32s3-espidf/debug/examples/list_selector_demo --monitor

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
use dynatac_core::keymap::KeyEvent;
use dynatac_core::list_selector::{ListAction, ListSelector};
use i2c_bus::I2cBus;
use keyboard::Keyboard;

const KEYBOARD_ADDR: u8 = 0x34;
const CHAR_W: usize = 8;
const CHAR_H: u16 = 8;
const COLS: usize = display::WIDTH as usize / CHAR_W;
/// Visible rows at the default 8×8 font. Update this (or derive it
/// dynamically) when font-size support is added.
const DISPLAY_ROWS: usize = display::HEIGHT as usize / CHAR_H as usize;

enum Screen {
    Intro(Option<String>),
    List(ListSelector),
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting list_selector_demo");

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
    sd_cs.set_high().unwrap();

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

    let i2c_driver = I2cDriver::new(
        peripherals.i2c0,
        pins.gpio13,
        pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    )
    .unwrap();
    let i2c_bus = I2cBus::new(i2c_driver);
    let mut kb = Keyboard::new(i2c_bus.device(KEYBOARD_ADDR)).unwrap();

    log::info!("Clearing display");
    epd.clear().unwrap();

    let mut screen = Screen::Intro(None);
    render(&mut epd, &screen);
    epd.try_flush().unwrap();

    loop {
        loop {
            match kb.poll() {
                Ok(Some(event)) => {
                    if handle_key(&mut screen, event) {
                        render(&mut epd, &screen);
                        epd.try_flush().unwrap();
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::error!("keyboard error: {:?}", e);
                    break;
                }
            }
        }
        sleep(Duration::from_millis(10));
    }
}

/// Returns true if the screen needs to be redrawn.
fn handle_key(screen: &mut Screen, event: KeyEvent) -> bool {
    match screen {
        Screen::Intro(_) => {
            if matches!(event, KeyEvent::Enter) {
                let items = (1u8..=50).map(|n| format!("Item {:02}", n)).collect();
                *screen = Screen::List(ListSelector::new("Choose an item:", items, DISPLAY_ROWS));
                true
            } else {
                false
            }
        }
        Screen::List(sel) => match sel.handle_key(event) {
            ListAction::Selected(item) => {
                *screen = Screen::Intro(Some(item));
                true
            }
            ListAction::Redraw => true,
            ListAction::None => false,
        },
    }
}

fn render(epd: &mut Epd, screen: &Screen) {
    clear_display(epd);
    match screen {
        Screen::Intro(last) => {
            draw_row(epd, 0, "=== DynaTac ===");
            draw_row(epd, 2, "Press Enter to open");
            draw_row(epd, 3, "the scrollable list.");
            draw_row(epd, 5, "y=up  h=down");
            draw_row(epd, 6, "Enter=select");
            if let Some(item) = last {
                draw_row(epd, 9, "You selected:");
                draw_row(epd, 10, item);
                draw_row(epd, 12, "Press Enter to go again.");
            }
        }
        Screen::List(sel) => {
            for (row, line) in sel.render().lines().enumerate() {
                draw_row(epd, row, line);
            }
        }
    }
}

/// Clear all display rows by writing white (space) across every row.
/// This updates the desired buffer so the dirty region covers any old content.
fn clear_display(epd: &mut Epd) {
    let mut y = 0u16;
    while y < display::HEIGHT {
        epd.clear_line(y);
        y += CHAR_H;
    }
}

/// Draw `text` at the given display row, padding the remainder of the row
/// with spaces so stale characters from a previous render are erased.
fn draw_row(epd: &mut Epd, row: usize, text: &str) {
    let y = row as u16 * CHAR_H;
    let chars: Vec<char> = text.chars().collect();
    for col in 0..COLS {
        let ch = chars.get(col).copied().unwrap_or(' ');
        epd.draw_char((col * CHAR_W) as u8, y, ch).unwrap();
    }
}
