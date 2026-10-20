//! Hardware check of the new modem stack (L0 transport → L1 link → L2 HAL,
//! via `src/modem_service.rs`). Serial output only.
//!
//!   ./flash-example.sh modem_hal_check
//!
//! 1. Powers the modem on and waits (≤ 60 s) for a `Configured` with no
//!    issues. Earlier passes may fail caller ID (SIM not ready yet); the
//!    log shows which boot milestone's re-run fixed it.
//! 2. Queries firmware, speaker gain and the call list.
//! 3. Asks you to call the device: answers on the second RING, holds the
//!    call 8 s, hangs up, then checks the call list is empty.
//! 4. Powers off: requests are refused at once (the driver doesn't stall
//!    during the 3 s PWRKEY pulse), the modem reports `ME DETACH`.
//!
//! Every notice is printed as it arrives. Ends with `CHECK PASS` or
//! `CHECK FAIL: …`.

#[path = "../src/modem_service.rs"]
mod modem_service;
#[path = "../src/modem_transport.rs"]
mod modem_transport;

use std::thread::sleep;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::peripherals::Peripherals;

use dynatac_core::modem::hal::{Event, RequestError, RequestId};
use dynatac_core::modem::port::{ModemPort, Notice};
use dynatac_core::modem::request::{Gain, Request, Response};
use dynatac_core::modem::unsolicited::Unsolicited;
use modem_service::ModemHandle;

const HOLD_CALL: Duration = Duration::from_secs(8);
const WAIT_FOR_CALL: Duration = Duration::from_secs(120);

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

struct Check {
    modem: ModemHandle,
    start: Instant,
}

impl Check {
    /// Next notice within `timeout`, printed.
    fn next(&mut self, timeout: Duration) -> Option<Notice> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(n) = self.modem.poll() {
                println!("[{:>6} ms] {n:?}", self.start.elapsed().as_millis());
                return Some(n);
            }
            sleep(Duration::from_millis(10));
        }
        None
    }

    /// Wait for a notice matching `pred`, printing everything on the way.
    fn wait_for(&mut self, what: &str, timeout: Duration, pred: impl Fn(&Notice) -> bool) -> Result<Notice, String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(n) = self.next(deadline - Instant::now()) {
                if pred(&n) {
                    return Ok(n);
                }
            }
        }
        Err(format!("timed out waiting for {what}"))
    }

    fn request(&mut self, r: Request) -> Result<Response, String> {
        let text = format!("{r:?}");
        println!("> {text}");
        let id: RequestId = self.modem.submit(r);
        match self.wait_for(&text, Duration::from_secs(30), |n| matches!(n, Notice::Done { id: i, .. } if *i == id))? {
            Notice::Done { result, .. } => result.map_err(|e: RequestError| format!("{text}: {e:?}")),
            _ => unreachable!(),
        }
    }
}

fn run() -> Result<(), String> {
    let p = Peripherals::take().unwrap();
    let modem = ModemHandle::start(p.uart1, p.pins.gpio10, p.pins.gpio11, p.pins.gpio40, p.pins.gpio41)
        .map_err(|e| format!("start: {e:?}"))?;
    let mut c = Check { modem, start: Instant::now() };

    println!("--- 1. power on, sync, configure");
    c.modem.power_on();
    // The first pass runs mid-boot, before the SIM is ready, so caller ID
    // may fail; the HAL re-runs at each boot milestone until clean.
    c.wait_for("Configured with no issues", Duration::from_secs(60), |n| {
        *n == Notice::Event(Event::Configured { issues: vec![] })
    })?;
    println!("configured, no issues");

    println!("--- 2. queries");
    let fw = c.request(Request::Firmware)?;
    println!("firmware: {fw:?}");
    if c.request(Request::QueryOutputGain)? != Response::Gain(Gain::MAX) {
        return Err("speaker gain is not 7".into());
    }
    if c.request(Request::ListCalls)? != Response::Calls(vec![]) {
        return Err("calls present before the test".into());
    }

    println!("--- 3. CALL THE DEVICE NOW (waiting up to {} s)", WAIT_FOR_CALL.as_secs());
    c.wait_for("first RING", WAIT_FOR_CALL, |n| matches!(n, Notice::Event(Event::Urc(Unsolicited::Ring))))?;
    c.wait_for("second RING", Duration::from_secs(10), |n| {
        matches!(n, Notice::Event(Event::Urc(Unsolicited::Ring)))
    })?;
    c.request(Request::Answer)?;
    println!("answered; holding {} s", HOLD_CALL.as_secs());
    let until = Instant::now() + HOLD_CALL;
    while Instant::now() < until {
        c.next(until - Instant::now());
    }
    c.request(Request::HangUp)?;
    sleep(Duration::from_millis(500));
    while c.next(Duration::from_millis(200)).is_some() {}
    if c.request(Request::ListCalls)? != Response::Calls(vec![]) {
        return Err("call still listed after hang-up".into());
    }

    println!("--- 4. power off");
    c.modem.power_off();
    let asked = Instant::now();
    let gone = c.modem.submit(Request::ListCalls);
    c.wait_for("NotSent after power-off", Duration::from_secs(10), |n| {
        matches!(n, Notice::Done { id, result: Err(RequestError::NotSent) } if *id == gone)
    })?;
    // The driver must not stall during the 3 s PWRKEY off-pulse.
    let took = asked.elapsed();
    println!("refused after {} ms", took.as_millis());
    if took > Duration::from_millis(500) {
        return Err(format!("driver stalled during the power-off pulse ({} ms)", took.as_millis()));
    }
    c.wait_for("ME DETACH", Duration::from_secs(10), |n| {
        matches!(n, Notice::Event(Event::Urc(Unsolicited::PacketDomain(d))) if d == "ME DETACH")
    })?;
    Ok(())
}
