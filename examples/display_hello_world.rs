//! Manual integration test: write "Hello World!" to the EPD one character at a time.
//!
//! Build:  cargo build --example display_hello_world
//! Flash:  espflash flash target/xtensa-esp32s3-espidf/debug/examples/display_hello_world --monitor

#[path = "../src/display.rs"]
mod display;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, SpiConfig, SpiDeviceDriver, SpiDriver},
    units::Hertz,
};

use display::Epd;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting");

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    // -------------------------------------------------------------------------
    // The LoRa module, SD card, and EPD share the same SPI bus.
    // Per the Arduino sketch, all CS lines must be driven HIGH before any SPI
    // traffic so devices don't interfere with each other.
    // -------------------------------------------------------------------------
    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();

    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();

    let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
    sd_cs.set_high().unwrap();

    // -------------------------------------------------------------------------
    // SPI2 bus: SCK=GPIO36, MOSI=GPIO33, no MISO needed for write-only EPD path
    // UC8253 spec: MODE0 (CPOL=0 CPHA=0), MSB first, up to 10 MHz
    // -------------------------------------------------------------------------
    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        pins.gpio36,                         // SCK
        pins.gpio33,                         // MOSI
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,  // no MISO
        &DriverConfig::new(),
    )
    .unwrap();

    let spi_config = SpiConfig::new().baudrate(Hertz(10_000_000));

    // CS is managed manually so that DC can be toggled around each command byte.
    let spi_device = SpiDeviceDriver::new(
        spi_driver,
        None::<AnyOutputPin>,
        &spi_config,
    )
    .unwrap();

    let cs   = PinDriver::output(pins.gpio34.downgrade_output()).unwrap();
    let dc   = PinDriver::output(pins.gpio35.downgrade_output()).unwrap();
    let busy = PinDriver::input(pins.gpio37.downgrade()).unwrap();

    let mut epd = Epd::new(spi_device, cs, dc, busy);

    // -------------------------------------------------------------------------
    // Write "Hello World!" one character at a time, 500 ms apart,
    // centred in the native 240×320 portrait frame.
    // -------------------------------------------------------------------------
    log::info!("Clearing display (full refresh ~1 s)");
    epd.clear().unwrap();

    let text = "Hello World!";
    let char_w: usize = 8;
    let char_h: usize = 8;

    let x_start: usize = (display::WIDTH as usize - text.len() * char_w) / 2;
    let y: u16        = (display::HEIGHT - char_h as u16) / 2;

    log::info!("Writing \"{}\" character by character", text);

    for (i, ch) in text.chars().enumerate() {
        let x = (x_start + i * char_w) as u8;
        log::info!("  '{}' at ({}, {})", ch, x, y);
        epd.draw_char(x, y, ch).unwrap();
        epd.flush_char(x, y).unwrap();
        sleep(Duration::from_millis(500));
    }

    log::info!("Done — powering down display");
    epd.power_down().unwrap();
}
