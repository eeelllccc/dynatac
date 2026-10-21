//! Does the 3.5mm jack's mic ring feed the A7682E's MIC input?
//!
//! Plug in a TRRS headset, then follow the on-screen prompts.
//! The test records 3 seconds from the A7682E's audio input and plays
//! it back through the headset speaker. If you hear your own voice,
//! the headset mic is wired to the A7682E and phone calls will work
//! fully in both directions.
//!
//! Build:
//!   cargo build -p dynatac --example headset_mic_test
//! Flash:
//!   espflash flash target/xtensa-esp32s3-espidf/debug/examples/headset_mic_test --monitor

#[path = "../src/display.rs"]
mod display;
#[path = "../src/i2c_bus.rs"]
mod i2c_bus;
#[path = "../src/keyboard.rs"]
mod keyboard;
#[path = "../src/modem.rs"]
mod modem;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, Dma, SpiConfig, SpiDeviceDriver, SpiDriver},
    uart::{config::Config as UartConfig, UartDriver},
    units::Hertz,
};

use display::Epd;
use dynatac_core::font::FontSize;
use dynatac_core::modem::Modem;
use i2c_bus::I2cBus;
use keyboard::Keyboard;
use modem::EspA7682EModem;

const KEYBOARD_ADDR: u8 = 0x34;
const FONT: FontSize = FontSize::Medium;
const ROW_HEIGHT: u16 = FONT.height();

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

    print_line(&mut epd, "=== Headset Mic ===", &mut y);
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Plug in a TRRS", &mut y);
    print_line(&mut epd, "headset (with mic),", &mut y);
    print_line(&mut epd, "then press any key.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Powering on modem...", &mut y);
    match modem.power_on() {
        Ok(()) => print_line(&mut epd, "[OK] Modem up.", &mut y),
        Err(e) => {
            print_line(&mut epd, &format!("[ERR] {:?}", e), &mut y);
            loop { sleep(Duration::from_secs(1)); }
        }
    }

    // --- Record ------------------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Press key, then speak", &mut y);
    print_line(&mut epd, "into headset mic for", &mut y);
    print_line(&mut epd, "3 seconds.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Recording...", &mut y);
    let r = modem.send_raw(r#"AT+CREC=1,"C:/h.wav""#);
    log::info!("CREC start: {:?}", r);
    sleep(Duration::from_secs(3));
    let r = modem.send_raw("AT+CREC=0");
    log::info!("CREC stop: {:?}", r);
    print_line(&mut epd, "Done.", &mut y);

    // --- Play back ---------------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Press key to play", &mut y);
    print_line(&mut epd, "back through headset.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Playing...", &mut y);
    let r = modem.send_raw(r#"AT+CCMXPLAY="C:/h.wav",0,1"#);
    log::info!("CCMXPLAY: {:?}", r);
    sleep(Duration::from_secs(5));

    // --- Result ------------------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Heard your voice?", &mut y);
    print_line(&mut epd, "  YES -> mic wired!", &mut y);
    print_line(&mut epd, "  NO  -> mic unwired", &mut y);
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Check serial log for", &mut y);
    print_line(&mut epd, "AT responses.", &mut y);
    print_line(&mut epd, "Press key to exit.", &mut y);
    wait_key(&mut kb);

    epd.power_down().unwrap();
}

fn print_line(epd: &mut Epd, text: &str, y: &mut u16) {
    if *y >= display::HEIGHT { return; }
    epd.clear_line(*y, ROW_HEIGHT);
    epd.draw_text(0, *y, FONT.font(), text);
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
