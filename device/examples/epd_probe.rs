//! Diagnostic: why did `spi_check` report success while the panel didn't
//! change? Its timings show BUSY never went low, so the panel ignored every
//! command. The prime suspect is the LoRa chip's state: `spi_check` holds
//! LoRa EN, CS and RST low (bus rule B5); the old firmware and LilyGo's
//! test_EPD hold CS and RST high. An unpowered SX1262 also sits on SCK and
//! MOSI, and may load them.
//!
//!   ./flash-example.sh epd_probe | tee captures/epd_probe.log
//!
//! Drives the wire directly (no `Bus`, which would refuse case C). For each
//! case: set the LoRa pins, init the panel, power it on (PON) and record
//! how long BUSY stays low; then power off (POF) the same way. A working
//! panel holds BUSY low for some milliseconds after PON. Then one full
//! draw in case B. LOOK: eight black/white bands with a border.

#[path = "../src/spi_wire.rs"]
mod spi_wire;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::delay::Ets;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;

use dynatac_core::framebuffer::{BUF_LEN, BYTES_PER_ROW, HEIGHT};
use dynatac_core::spi::wire::{Hz, Input, Output, Wire};
use spi_wire::{EspWire, Pins};

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let mut p = Peripherals::take().unwrap();

    // What the board does with GPIO46 when nothing drives it, i.e. whether
    // LoRa was powered under the old firmware.
    {
        let en = PinDriver::input(&mut p.pins.gpio46).unwrap();
        println!("PROBE gpio46 (LoRa EN) undriven reads {}", level(en.is_high()));
    }

    let pins = Pins {
        sck: p.pins.gpio36,
        mosi: p.pins.gpio33,
        miso: p.pins.gpio47,
        epd_cs: p.pins.gpio34,
        epd_dc: p.pins.gpio35,
        epd_busy: p.pins.gpio37,
        sd_cs: p.pins.gpio48,
        lora_cs: p.pins.gpio3,
        lora_rst: p.pins.gpio4,
        lora_busy: p.pins.gpio6,
        lora_dio1: p.pins.gpio5,
        lora_en: p.pins.gpio46,
    };
    let mut w = EspWire::new(p.spi2, pins).unwrap();
    w.set(Output::EpdCs, true);
    w.set(Output::SdCs, true);
    w.set(Output::EpdDc, true);
    println!("PROBE epd busy at start reads {}", level(w.get(Input::EpdBusy)));

    // (name, LoRa EN, CS, RST, clock)
    let cases = [
        ("A  lora off, cs/rst low (spi_check)", false, false, false, 5_000_000),
        ("A' same at 10 MHz (old driver clock)", false, false, false, 10_000_000),
        ("C  lora off, cs/rst high (old firmware)", false, true, true, 5_000_000),
        ("B  lora on, cs/rst high", true, true, true, 5_000_000),
        ("A2 lora off, cs/rst low again", false, false, false, 5_000_000),
    ];
    for (name, en, cs, rst, clock) in cases {
        w.set(Output::LoraEn, en);
        w.set(Output::LoraCs, cs);
        w.set(Output::LoraRst, rst);
        sleep(Duration::from_millis(50));
        let lora_busy = w.get(Input::LoraBusy);
        let clock = Hz(clock);
        idle_clocks(&mut w, clock);
        init(&mut w, clock);
        cmd(&mut w, clock, 0x04, &[]);
        let pon = busy_low(&mut w, 500);
        cmd(&mut w, clock, 0x02, &[]);
        let pof = busy_low(&mut w, 500);
        println!("PROBE {name}: lora busy {} · PON {} · POF {}", level(lora_busy), pon, pof);
    }

    // A visible full draw in case B.
    w.set(Output::LoraEn, true);
    w.set(Output::LoraCs, true);
    w.set(Output::LoraRst, true);
    sleep(Duration::from_millis(50));
    let clock = Hz(5_000_000);
    init(&mut w, clock);
    cmd(&mut w, clock, 0x10, &vec![0xFF; BUF_LEN]);
    cmd(&mut w, clock, 0x13, &test_image());
    cmd(&mut w, clock, 0xE0, &[0x02]);
    cmd(&mut w, clock, 0xE5, &[0x5A]);
    cmd(&mut w, clock, 0x50, &[0x97]);
    cmd(&mut w, clock, 0x04, &[]);
    let pon = busy_low(&mut w, 500);
    cmd(&mut w, clock, 0x12, &[]);
    let refresh = busy_low(&mut w, 10_000);
    cmd(&mut w, clock, 0x02, &[]);
    let pof = busy_low(&mut w, 500);
    println!("PROBE full draw (case B): PON {pon} · refresh {refresh} · POF {pof}");
    println!("PROBE done. LOOK: eight equal bands with a border?");
    loop {
        sleep(Duration::from_secs(60));
    }
}

fn level(high: bool) -> &'static str {
    if high { "high" } else { "low" }
}

fn idle_clocks(w: &mut EspWire, clock: Hz) {
    w.transfer(clock, &[0xFF; 10], None).unwrap();
}

/// One command, then its parameters, each in its own CS frame (as the old
/// driver and GxEPD2 do). Data goes in chunks that fit the DMA buffer.
fn cmd(w: &mut EspWire, clock: Hz, c: u8, data: &[u8]) {
    w.set(Output::EpdDc, false);
    w.set(Output::EpdCs, false);
    w.transfer(Hz(clock.0), &[c], None).unwrap();
    w.set(Output::EpdCs, true);
    w.set(Output::EpdDc, true);
    for chunk in data.chunks(4092) {
        w.set(Output::EpdCs, false);
        w.transfer(Hz(clock.0), chunk, None).unwrap();
        w.set(Output::EpdCs, true);
    }
}

/// Soft reset and panel setting, as `core::spi::epaper`.
fn init(w: &mut EspWire, clock: Hz) {
    cmd(w, Hz(clock.0), 0x00, &[0x1E, 0x0D]);
    let reset = busy_low(w, 100);
    cmd(w, Hz(clock.0), 0x00, &[0x1F, 0x0D]);
    println!("PROBE   soft reset: {reset}");
}

/// Sample BUSY every 100 µs for up to `limit_ms`: when it first went low,
/// and how long it stayed low.
fn busy_low(w: &mut EspWire, limit_ms: u32) -> String {
    let mut fell = None;
    for tick in 0..limit_ms * 10 {
        let low = !w.get(Input::EpdBusy);
        match (fell, low) {
            (None, true) => fell = Some(tick),
            (Some(f), false) => return format!("BUSY low at {:.1} ms for {:.1} ms", f as f32 / 10.0, (tick - f) as f32 / 10.0),
            _ => {}
        }
        Ets::delay_us(100);
        if tick % 1000 == 999 {
            sleep(Duration::from_millis(10)); // let the idle task feed the watchdog
        }
    }
    match fell {
        None => "BUSY never low".into(),
        Some(f) => format!("BUSY low at {:.1} ms, still low at {limit_ms} ms", f as f32 / 10.0),
    }
}

/// Eight horizontal bands plus a 2-pixel border, as in `spi_check`.
fn test_image() -> Vec<u8> {
    let mut img = vec![0xFF; BUF_LEN];
    let band = HEIGHT as usize / 8;
    for y in 0..HEIGHT as usize {
        let row = &mut img[y * BYTES_PER_ROW..(y + 1) * BYTES_PER_ROW];
        if (y / band) % 2 == 0 || y < 2 || y >= HEIGHT as usize - 2 {
            row.fill(0x00);
        }
        row[0] &= 0x3F;
        row[BYTES_PER_ROW - 1] &= 0xFC;
    }
    img
}
