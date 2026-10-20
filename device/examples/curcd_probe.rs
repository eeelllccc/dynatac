//! Hardware check for the resync probe: does `AT+CURCD` (URC delay, 0–10000 ms,
//! NO_SAVE) work on this firmware, before the SIM is ready, and in a combined
//! write+read command line? Raw UART, serial output only.
//!
//!   ./flash-example.sh curcd_probe
//!
//! Powers the modem on, waits for `*ATREADY`, then immediately (SIM not ready
//! yet) and again after `PB DONE` runs:
//!   ATE0, AT+CURCD=?, AT+CURCD?, AT+CURCD=37,1, AT+CURCD?,
//!   AT+CURCD=38,1;+CURCD?   (combined line)
//!   AT+CURCD=0,1;+CURCD?
//! and prints every received line with its time.

#[path = "../src/modem_transport.rs"]
mod modem_transport;

use std::thread::sleep;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::{
    gpio::{OutputPin, PinDriver},
    peripherals::Peripherals,
};

use dynatac_core::modem::at::{AtEvent, AtParser};
use dynatac_core::modem::transport::{Rx, Transport};
use modem_transport::UartTransport;

const COMMANDS: &[&str] = &[
    "ATE0",
    "AT+CURCD=?",
    "AT+CURCD?",
    "AT+CURCD=37,1",
    "AT+CURCD?",
    "AT+CURCD=38,1;+CURCD?",
    "AT+CURCD=0,1;+CURCD?",
];

struct Probe {
    t: UartTransport,
    parser: AtParser,
    start: Instant,
}

impl Probe {
    /// Print lines for `ms`; returns them.
    fn listen(&mut self, ms: u64) -> Vec<String> {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut lines = Vec::new();
        while Instant::now() < end {
            match self.t.read(Duration::from_millis(20)) {
                Some(Rx::Data(d)) => {
                    for e in self.parser.feed(&d) {
                        let l = match e {
                            AtEvent::Line(l) => l,
                            AtEvent::Prompt => "> ".into(),
                        };
                        println!("[{:>6} ms] < {l}", self.start.elapsed().as_millis());
                        lines.push(l);
                    }
                }
                Some(Rx::Lost) => println!("[{:>6} ms] < LOST", self.start.elapsed().as_millis()),
                None => {}
            }
        }
        lines
    }

    /// Send one command line and collect lines until a final code (or 3 s).
    fn command(&mut self, cmd: &str) -> Vec<String> {
        println!("[{:>6} ms] > {cmd}", self.start.elapsed().as_millis());
        self.t.write(format!("{cmd}\r").as_bytes()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        let mut got = Vec::new();
        while Instant::now() < end {
            let lines = self.listen(50);
            let done = lines.iter().any(|l| l == "OK" || l.contains("ERROR"));
            got.extend(lines);
            if done {
                break;
            }
        }
        got
    }

    fn run_all(&mut self, phase: &str) {
        println!("--- {phase}");
        for c in COMMANDS {
            let r = self.command(c);
            println!("    => {r:?}");
        }
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let p = Peripherals::take().unwrap();
    let mut power_en = PinDriver::output(p.pins.gpio41.downgrade_output()).unwrap();
    let mut pwrkey = PinDriver::output(p.pins.gpio40.downgrade_output()).unwrap();
    let t = UartTransport::start(p.uart1, p.pins.gpio10, p.pins.gpio11, 4096).unwrap();
    let mut probe = Probe { t, parser: AtParser::new(), start: Instant::now() };

    power_en.set_high().unwrap();
    sleep(Duration::from_millis(100));
    pwrkey.set_high().unwrap();
    sleep(Duration::from_secs(1));
    pwrkey.set_low().unwrap();

    println!("--- waiting for *ATREADY");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if probe.listen(50).iter().any(|l| l == "*ATREADY: 1") {
            break;
        }
    }
    probe.run_all("right after *ATREADY (SIM not ready yet)");

    println!("--- waiting for PB DONE");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if probe.listen(50).iter().any(|l| l == "PB DONE") {
            break;
        }
    }
    probe.run_all("after PB DONE");
    println!("PROBE DONE");
    loop {
        sleep(Duration::from_secs(60));
    }
}
