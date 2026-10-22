//! Hardware check of the new SPI stack (core `spi::{bus, epaper, scheduler}`
//! via `src/spi_service.rs` and `src/spi_wire.rs`). Serial output, plus
//! two things to look at on the screen.
//!
//!   ./flash-example.sh spi_check | tee captures/spi_check.log
//!
//! Checks (docs/spi-design.md, "Hardware: what needs testing"):
//! 1. Bus bring-up and the actual SPI clocks (logged as `spi: device for …`).
//! 2. A full draw at 5 MHz (the e-paper's spec limit; the old driver used
//!    10 MHz) of a striped test image. The image is sent in chunks with CS
//!    raised between them: if chunking broke the controller's data stream,
//!    the stripes would be shifted or torn. LOOK: eight black/white bands
//!    of equal height, then a thin black border round the screen.
//! 3. Twenty partial draws, each adding a black square a step further
//!    down and right (nothing is erased), each timed. If BUSY interrupts
//!    didn't wake the spi task, each would take its multi-second deadline
//!    instead of ~0.7 s. LOOK: a diagonal staircase of overlapping black
//!    squares over the bands, edges crisp.
//! 4. Power off.
//!
//! Prints every response with its time, then a timing summary and
//! `CHECK PASS` or `CHECK FAIL: …`.

#[path = "../src/spi_service.rs"]
mod spi_service;
#[path = "../src/spi_wire.rs"]
mod spi_wire;

use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::peripherals::Peripherals;

use dynatac_core::framebuffer::{BUF_LEN, BYTES_PER_ROW, HEIGHT};
use dynatac_core::kernel::{Event, SpiPort};
use dynatac_core::spi::epaper::{Done, Request as Epd, Window};
use dynatac_core::spi::scheduler::{Request, Response};
use spi_service::SpiHandle;
use spi_wire::Pins;

/// A partial draw slower than this means the BUSY interrupt didn't wake
/// the task (it slept to the deadline).
const PARTIAL_LIMIT: Duration = Duration::from_millis(2_000);
const FULL_LIMIT: Duration = Duration::from_millis(5_000);
/// A draw faster than this never saw BUSY low: the panel ignored it.
/// (Measured by `epd_probe`: a full refresh holds BUSY low ~0.9 s.)
const PARTIAL_MIN: Duration = Duration::from_millis(100);
const FULL_MIN: Duration = Duration::from_millis(400);

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    match run() {
        Ok(()) => println!("CHECK PASS"),
        Err(e) => println!("CHECK FAIL: {e}"),
    }
    loop {
        sleep(Duration::from_secs(60));
    }
}

fn run() -> Result<(), String> {
    let p = Peripherals::take().map_err(|e| format!("{e:?}"))?;
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
    let (to_me, inbox) = mpsc::channel();
    let mut spi = SpiHandle::start(p.spi2, pins, to_me).map_err(|e| format!("start: {e:?}"))?;
    let start = Instant::now();

    let mut draw = |request: Epd, min: Duration, limit: Duration| -> Result<Duration, String> {
        let t0 = Instant::now();
        let id = spi.submit(Request::Epaper(request));
        loop {
            if let Ok(Event::Spi(got, response)) = inbox.recv_timeout(Duration::from_millis(5)) {
                let took = t0.elapsed();
                println!("[{:>6} ms] {got:?} {response:?} after {} ms", start.elapsed().as_millis(), took.as_millis());
                if got != id {
                    return Err(format!("response for {got:?}, expected {id:?}"));
                }
                return match response {
                    Response::Epaper(Done::Drawn(Ok(()))) if took < min => Err(format!("took only {} ms: the panel never went busy (ignoring us?)", took.as_millis())),
                    Response::Epaper(Done::Drawn(Ok(()))) | Response::Epaper(Done::PoweredOff(Ok(()))) if took <= limit => Ok(took),
                    Response::Epaper(Done::Drawn(Ok(()))) => Err(format!("took {} ms (BUSY interrupt not waking the task?)", took.as_millis())),
                    other => Err(format!("{other:?}")),
                };
            }
            if t0.elapsed() > Duration::from_secs(30) {
                return Err("no response within 30 s".into());
            }
        }
    };

    let full = draw(Epd::Full { data: test_image() }, FULL_MIN, FULL_LIMIT)?;

    let side = 32u16;
    let mut partials = Vec::new();
    for k in 0..20u16 {
        let window = Window { x: (k * 8) % (240 - side), y: k * 14, w: side, h: side };
        // Clear the previous square's spot and draw the new one in one window
        // would need a bigger window; keep it simple: one black square each.
        partials.push(draw(Epd::Partial { window, data: vec![0x00; window.data_len()] }, PARTIAL_MIN, PARTIAL_LIMIT)?);
    }
    let off = draw(Epd::PowerOff, Duration::ZERO, PARTIAL_LIMIT)?;

    let ms = |d: &Duration| d.as_millis();
    let max = partials.iter().map(ms).max().unwrap_or(0);
    let avg = partials.iter().map(ms).sum::<u128>() / partials.len() as u128;
    println!("TIMING full {} ms · partial avg {avg} ms, max {max} ms · power off {} ms", ms(&full), ms(&off));
    println!("LOOK: eight equal bands with a border; a trail of black squares, no ghosting.");
    Ok(())
}

/// Eight horizontal bands, alternately black and white, plus a 2-pixel
/// black border. Band edges fall inside different DMA chunks, so a torn
/// or shifted chunk shows up as a broken band.
fn test_image() -> Vec<u8> {
    let mut img = vec![0xFF; BUF_LEN];
    let band = HEIGHT as usize / 8;
    for y in 0..HEIGHT as usize {
        let row = &mut img[y * BYTES_PER_ROW..(y + 1) * BYTES_PER_ROW];
        if (y / band) % 2 == 0 {
            row.fill(0x00);
        }
        if y < 2 || y >= HEIGHT as usize - 2 {
            row.fill(0x00);
        }
        row[0] &= 0x3F;
        row[BYTES_PER_ROW - 1] &= 0xFC;
    }
    img
}
