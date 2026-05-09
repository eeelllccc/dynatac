//! Integration test: SD card write → read → delete.
//!
//! Step through a write/read/delete cycle on /sdcard/hello.txt.
//! Each step is gated by a keypress so you can read the result on screen
//! and in the serial monitor before continuing.
//!
//! Prerequisites:
//!   - A FAT32-formatted microSD card must be inserted before boot.
//!     (On macOS: Disk Utility → Erase → MS-DOS (FAT), ≤32 GB card.)
//!
//! Build:  cargo build -p dynatac --example sd_card_test
//! Flash:  espflash flash target/xtensa-esp32s3-espidf/debug/examples/sd_card_test --monitor

#[path = "../src/display.rs"]
mod display;
#[path = "../src/i2c_bus.rs"]
mod i2c_bus;
#[path = "../src/keyboard.rs"]
mod keyboard;
#[path = "../src/sdcard.rs"]
mod sdcard;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, Dma, SpiConfig, SpiDeviceDriver, SpiDriver},
    units::Hertz,
};

use display::Epd;
use dynatac_core::fs::FileSystem;
use i2c_bus::I2cBus;
use keyboard::Keyboard;
use sdcard::SdCardFs;

const KEYBOARD_ADDR: u8 = 0x34;
const ROW_HEIGHT: u16 = 10;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting sd_card_test");

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    // --- Deselect other SPI devices on the shared bus -------------------------
    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    {
        // Drive SD CS high during SPI bus init, then release so the SDSPI
        // driver can take ownership of GPIO48 as its chip-select.
        let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
        sd_cs.set_high().unwrap();
    }

    // --- SPI bus (shared: EPD + SD card) -------------------------------------
    // MISO (GPIO47) is needed by the SD card; the EPD ignores it.
    // DMA::Auto(4096): SDSPI needs >64-byte transfers; Dma::Disabled caps at 64.
    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        pins.gpio36,
        pins.gpio33,
        Some(pins.gpio47),
        &DriverConfig::new().dma(Dma::Auto(4096)),
    )
    .unwrap();

    let spi_device = SpiDeviceDriver::new(
        spi_driver,
        None::<AnyOutputPin>,
        &SpiConfig::new().baudrate(Hertz(10_000_000)),
    )
    .unwrap();

    let cs   = PinDriver::output(pins.gpio34.downgrade_output()).unwrap();
    let dc   = PinDriver::output(pins.gpio35.downgrade_output()).unwrap();
    let busy = PinDriver::input(pins.gpio37.downgrade()).unwrap();

    let mut epd = Epd::new(spi_device, cs, dc, busy);

    // --- I2C bus (keyboard) --------------------------------------------------
    let i2c_driver = I2cDriver::new(
        peripherals.i2c0,
        pins.gpio13,
        pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    )
    .unwrap();
    let i2c_bus = I2cBus::new(i2c_driver);
    let mut kb = Keyboard::new(i2c_bus.device(KEYBOARD_ADDR)).unwrap();

    // --- Init display --------------------------------------------------------
    log::info!("Clearing display");
    epd.clear().unwrap();

    // --- Intro ---------------------------------------------------------------
    let mut y: u16 = 0;
    print_line(&mut epd, "=== SD Card Test ===", &mut y);
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Writes, reads, deletes", &mut y);
    print_line(&mut epd, "  /sdcard/hello.txt", &mut y);
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Insert a FAT32 card,", &mut y);
    print_line(&mut epd, "then press any key.", &mut y);
    wait_key(&mut kb);

    // --- Step 1: Mount -------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Mounting...", &mut y);
    let mut fs = match SdCardFs::mount() {
        Ok(fs) => {
            log::info!("SD mounted OK");
            print_line(&mut epd, "  [OK] Mounted.", &mut y);
            fs
        }
        Err(e) => {
            log::error!("SD mount failed: {}", e);
            print_line(&mut epd, "  [ERR] Mount failed.", &mut y);
            print_line(&mut epd, "Check card and reset.", &mut y);
            loop { sleep(Duration::from_secs(1)); }
        }
    };

    // --- Step 2: Write -------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Press any key to write.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Writing hello.txt...", &mut y);
    match fs.write("hello.txt", b"Hello, World!\n") {
        Ok(()) => {
            log::info!("Write OK");
            print_line(&mut epd, "  [OK] Written.", &mut y);
        }
        Err(e) => {
            log::error!("Write failed: {}", e);
            print_line(&mut epd, &format!("  [ERR] {}", e), &mut y);
        }
    }

    // --- Step 3: Read --------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Press any key to read.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Reading hello.txt...", &mut y);
    match fs.read_str("hello.txt") {
        Ok(s) => {
            log::info!("Read OK: {:?}", s);
            print_line(&mut epd, "  [OK] Content:", &mut y);
            print_line(&mut epd, &format!("  '{}'", s.trim()), &mut y);
        }
        Err(e) => {
            log::error!("Read failed: {}", e);
            print_line(&mut epd, &format!("  [ERR] {}", e), &mut y);
        }
    }

    // --- Step 4: Delete ------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Press key to delete.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Deleting hello.txt...", &mut y);
    match fs.delete("hello.txt") {
        Ok(()) => {
            log::info!("Delete OK");
            print_line(&mut epd, "  [OK] Deleted.", &mut y);
        }
        Err(e) => {
            log::error!("Delete failed: {}", e);
            print_line(&mut epd, &format!("  [ERR] {}", e), &mut y);
        }
    }

    if fs.exists("hello.txt") {
        log::warn!("exists() returned true after delete");
        print_line(&mut epd, "  [ERR] Still exists!", &mut y);
    } else {
        log::info!("exists() correctly false after delete");
        print_line(&mut epd, "  Gone: confirmed.", &mut y);
    }

    // --- Done ----------------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "All done.", &mut y);
    print_line(&mut epd, "Press any key to exit.", &mut y);
    wait_key(&mut kb);

    epd.power_down().unwrap();
}

/// Draw `text` at the current row, flush, and advance `y` by one row height.
/// Truncated to 30 characters (240 px / 8 px per glyph).
/// Silently no-ops if `y` has reached the bottom of the display.
fn print_line(epd: &mut Epd, text: &str, y: &mut u16) {
    if *y >= display::HEIGHT {
        return;
    }
    epd.clear_line(*y);
    for (i, ch) in text.chars().take(30).enumerate() {
        epd.draw_char((i * 8) as u8, *y, ch).unwrap();
    }
    if let Err(e) = epd.try_flush() {
        log::error!("display flush: {:?}", e);
    }
    *y += ROW_HEIGHT;
}

/// Block until any key is pressed.
fn wait_key(kb: &mut Keyboard) {
    loop {
        match kb.poll() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(e) => log::warn!("kb poll: {:?}", e),
        }
        sleep(Duration::from_millis(10));
    }
}
