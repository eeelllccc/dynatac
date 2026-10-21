//! Find the correct AT+CMIC channel and gain level for the headset mic.
//!
//! AT+CMIC=0,10 errored in earlier tests. This sweeps through channels 0–2
//! at several gain levels, recording and playing back at each so you can
//! hear which combination is loudest/clearest. The passing combination gives
//! us the right values to hardcode in the phone call implementation.
//!
//! Plug in a TRRS headset before starting.
//!
//! Build:
//!   cargo build -p dynatac --example mic_gain_sweep
//! Flash:
//!   espflash flash target/xtensa-esp32s3-espidf/debug/examples/mic_gain_sweep --monitor

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

// Candidates to sweep: (channel, gain).
// Channel: 0=main mic, 1=headset mic, 2=aux (per A76XX docs).
// Gain: 0–15 range (higher = louder).
const SWEEP: &[(u8, u8)] = &[
    (0, 15),
    (1, 15),
    (2, 15),
    (1, 10),
    (1,  5),
];

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

    print_line(&mut epd, "=== Mic Gain Sweep ===", &mut y);
    print_line(&mut epd, "Plug in TRRS headset", &mut y);
    print_line(&mut epd, "then press any key.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Powering on modem...", &mut y);
    match modem.power_on() {
        Ok(()) => print_line(&mut epd, "[OK] Modem up.", &mut y),
        Err(e) => {
            print_line(&mut epd, &format!("[ERR] {:?}", e), &mut y);
            loop { sleep(Duration::from_secs(1)); }
        }
    }

    // Also query AT+CMICGAIN which some A76XX firmware uses instead of AT+CMIC
    let cmicgain_q = modem.send_raw("AT+CMICGAIN?");
    log::info!("AT+CMICGAIN?: {:?}", cmicgain_q);

    for &(ch, gain) in SWEEP {
        epd.clear().unwrap();
        y = 0;

        print_line(&mut epd, &format!("CMIC ch={} gain={}", ch, gain), &mut y);
        print_line(&mut epd, "", &mut y);

        // Apply gain setting — log whether it was accepted or rejected.
        // Some firmware uses AT+CMIC, others AT+CMICGAIN.
        let cmd = format!("AT+CMIC={},{}", ch, gain);
        let r_cmic = modem.send_raw(&cmd);
        log::info!("{}: {:?}", cmd, r_cmic);

        let cmd2 = format!("AT+CMICGAIN={}", gain);
        let r_cmicgain = modem.send_raw(&cmd2);
        log::info!("{}: {:?}", cmd2, r_cmicgain);

        let cmic_ok = r_cmic.is_ok();
        let cmicgain_ok = r_cmicgain.is_ok();
        print_line(&mut epd, &format!("CMIC:     {}", ok_str(cmic_ok)), &mut y);
        print_line(&mut epd, &format!("CMICGAIN: {}", ok_str(cmicgain_ok)), &mut y);
        print_line(&mut epd, "", &mut y);

        // Record 3 s then play back.
        print_line(&mut epd, "Press key, speak 3s.", &mut y);
        wait_key(&mut kb);

        print_line(&mut epd, "Recording...", &mut y);
        let _ = modem.send_raw(r#"AT+CREC=1,"C:/g.wav""#);
        sleep(Duration::from_secs(3));
        let _ = modem.send_raw("AT+CREC=0");

        print_line(&mut epd, "Playing back...", &mut y);
        let _ = modem.send_raw(r#"AT+CCMXPLAY="C:/g.wav",0,1"#);
        sleep(Duration::from_secs(4));

        print_line(&mut epd, "Loud enough?", &mut y);
        print_line(&mut epd, "Key=next  (note vol)", &mut y);
        wait_key(&mut kb);
    }

    epd.clear().unwrap();
    y = 0;
    print_line(&mut epd, "=== Sweep done ===", &mut y);
    print_line(&mut epd, "Check serial log for", &mut y);
    print_line(&mut epd, "which CMIC cmds were", &mut y);
    print_line(&mut epd, "accepted (OK vs ERR).", &mut y);
    print_line(&mut epd, "Note the ch+gain that", &mut y);
    print_line(&mut epd, "sounded loudest.", &mut y);
    print_line(&mut epd, "Press key to exit.", &mut y);
    wait_key(&mut kb);

    epd.power_down().unwrap();
}

fn ok_str(ok: bool) -> &'static str {
    if ok { "[OK]" } else { "[ERR]" }
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
