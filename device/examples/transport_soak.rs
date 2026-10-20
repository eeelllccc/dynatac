//! Hardware test of the L0 modem transport (`src/modem_transport.rs`).
//! Serial output only; no keyboard or display needed.
//!
//!   ./flash-example.sh transport_soak
//!
//! 1. Powers the modem on and waits for `*ATREADY: 1`, then `ATE0`.
//! 2. Integrity: sends `ATI` 300 times, reading continuously. Every reply
//!    must be byte-identical to the first and no loss may be reported.
//! 3. Loss detection: sends `ATI` 6 times without reading (~900 bytes into
//!    a 512-byte pipe). Exactly one loss must be reported, after exactly
//!    the bytes that fit, and those bytes must be the true stream prefix.
//!
//! Ends with `SOAK PASS` or `SOAK FAIL: …`.

#[path = "../src/modem_transport.rs"]
mod modem_transport;

use std::thread::sleep;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::{
    gpio::{OutputPin, PinDriver},
    peripherals::Peripherals,
};

use dynatac_core::modem::transport::{Rx, Transport};
use modem_transport::UartTransport;

const PIPE: usize = 512;
const ROUNDS: usize = 300;
const BURST: usize = 6;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    match run() {
        Ok(()) => println!("SOAK PASS"),
        Err(e) => println!("SOAK FAIL: {e}"),
    }
    loop {
        sleep(Duration::from_secs(60));
    }
}

fn run() -> Result<(), String> {
    let p = Peripherals::take().unwrap();
    let mut power_en = PinDriver::output(p.pins.gpio41.downgrade_output()).unwrap();
    let mut pwrkey = PinDriver::output(p.pins.gpio40.downgrade_output()).unwrap();
    let mut t = UartTransport::start(p.uart1, p.pins.gpio10, p.pins.gpio11, PIPE)
        .map_err(|e| format!("uart: {e:?}"))?;

    // Power on (T-Deck-Pro inverts PWRKEY: HIGH presses the button).
    power_en.set_high().unwrap();
    sleep(Duration::from_millis(100));
    pwrkey.set_high().unwrap();
    sleep(Duration::from_secs(1));
    pwrkey.set_low().unwrap();
    println!("waiting for *ATREADY");
    // If the modem was already on, there is no banner; AT then answers.
    let banner = read_until(&mut t, b"*ATREADY: 1\r\n", Duration::from_secs(15))?;
    if !banner {
        println!("no banner (modem already on?)");
    }
    // Let the boot URCs (CPIN, SMS DONE, PB DONE…) pass before measuring.
    sleep(Duration::from_secs(15));
    drain(&mut t);

    command(&mut t, "ATE0")?;
    sleep(Duration::from_millis(200));
    drain(&mut t);

    // --- integrity ---
    let reference = command(&mut t, "ATI")?;
    println!("reference ATI reply: {} bytes", reference.len());
    let start = Instant::now();
    for i in 0..ROUNDS {
        let reply = command(&mut t, "ATI")?;
        if reply != reference {
            return Err(format!(
                "round {i}: reply differs\n  want {:?}\n  got  {:?}",
                String::from_utf8_lossy(&reference),
                String::from_utf8_lossy(&reply)
            ));
        }
    }
    println!(
        "integrity: {ROUNDS} identical replies, {} bytes, no loss, {} ms",
        ROUNDS * reference.len(),
        start.elapsed().as_millis()
    );

    // --- loss detection ---
    for _ in 0..BURST {
        t.write(b"ATI\r").map_err(|e| format!("write: {e:?}"))?;
        sleep(Duration::from_millis(300)); // not reading: the pipe fills
    }
    sleep(Duration::from_millis(500));
    let mut before = Vec::new();
    let mut losses = 0;
    let mut after = 0;
    while let Some(rx) = t.read(Duration::from_millis(300)) {
        match rx {
            Rx::Data(d) if losses == 0 => before.extend(d),
            Rx::Data(d) => after += d.len(),
            Rx::Lost => losses += 1,
        }
    }
    let expected: Vec<u8> = reference.repeat(BURST);
    println!(
        "burst: sent {} bytes worth, got {} before loss, {losses} loss marker(s), {after} after",
        expected.len(),
        before.len()
    );
    if losses != 1 {
        return Err(format!("expected exactly 1 loss marker, got {losses}"));
    }
    if before.len() != PIPE {
        return Err(format!("expected {PIPE} bytes before the loss, got {}", before.len()));
    }
    if !expected.starts_with(&before) {
        return Err("bytes before the loss are not the true stream prefix".into());
    }
    if after != 0 {
        return Err(format!("{after} bytes after the loss; expected none (all sent while full)"));
    }
    Ok(())
}

/// Send a command and collect its reply bytes up to and including
/// `OK\r\n`. Any loss, `ERROR` or 2 s silence is a failure.
fn command(t: &mut UartTransport, cmd: &str) -> Result<Vec<u8>, String> {
    t.write(format!("{cmd}\r").as_bytes()).map_err(|e| format!("write: {e:?}"))?;
    let mut reply = Vec::new();
    loop {
        match t.read(Duration::from_secs(2)) {
            Some(Rx::Data(d)) => reply.extend(d),
            Some(Rx::Lost) => return Err(format!("{cmd}: loss reported")),
            None => {
                return Err(format!("{cmd}: timeout, got {:?}", String::from_utf8_lossy(&reply)))
            }
        }
        if reply.ends_with(b"\r\nOK\r\n") {
            return Ok(reply);
        }
        if reply.ends_with(b"ERROR\r\n") {
            return Err(format!("{cmd}: ERROR"));
        }
    }
}

/// Read until `needle` has been seen or `timeout` passes.
fn read_until(t: &mut UartTransport, needle: &[u8], timeout: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        match t.read(Duration::from_millis(100)) {
            Some(Rx::Data(d)) => {
                seen.extend(d);
                if seen.windows(needle.len()).any(|w| w == needle) {
                    return Ok(true);
                }
            }
            Some(Rx::Lost) => return Err("loss during boot".into()),
            None => {}
        }
    }
    Ok(false)
}

fn drain(t: &mut UartTransport) {
    while let Some(rx) = t.read(Duration::from_millis(100)) {
        if rx == Rx::Lost {
            println!("(loss while draining)");
        }
    }
}
