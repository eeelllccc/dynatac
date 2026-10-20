// The modem stack on the device: a driver thread that owns the UART
// transport (L0), the HAL (L1 + L2) and the modem power pins, and a
// `ModemHandle` that implements `ModemPort` over channels. Code written
// against `ModemPort` (and tested with `MockHal` / `SimModem` in core)
// runs unchanged on the device.
//
// Hardware connections:
//   UART1 (see `modem_transport.rs`)
//   PWRKEY   → GPIO40 (inverted on the board: HIGH presses the button)
//   POWER_EN → GPIO41 (modem supply)
//
// The driver thread is a thin loop around `dynatac_core::modem::service::
// Service` (HAL + power sequencing), which the core tests drive against
// the simulator. It never sleeps: power pulses are timed steps, so the
// link keeps being served while PWRKEY is held (up to 3 s), and commands
// take effect in order (a request after `power_off` is refused).
//
// Caller invariants:
//   - One `ModemHandle` per modem; it owns UART1 and GPIO40/41.
//   - Poll the handle regularly: notices queue up until read.
//
// Callee invariants (the `ModemPort` contract):
//   - Exactly one `Notice::Done` per submitted request, in order.
//   - Requests submitted before `power_on` (or after `power_off`) are
//     answered `NotSent`.
//   - Request latency added by the thread hop is at most `STEP_MS`, also
//     during power sequences.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use esp_idf_svc::hal::gpio::{AnyOutputPin, Gpio10, Gpio11, Gpio40, Gpio41, Output, OutputPin, PinDriver};
use esp_idf_svc::hal::uart::UART1;
use esp_idf_svc::sys::EspError;

use dynatac_core::modem::hal::{self, RequestId};
use dynatac_core::modem::port::{ModemPort, Notice};
use dynatac_core::modem::request::Request;
use dynatac_core::modem::service::{Pin as ServicePin, Service};
use dynatac_core::modem::transport::Clock;

use crate::modem_transport::UartTransport;

/// Received bytes that may wait for the driver thread before loss.
const PIPE_CAPACITY: usize = 16 * 1024;
/// Longest the driver thread waits for modem input before checking for
/// new requests.
const STEP_MS: u64 = 20;

enum ToDriver {
    Submit(RequestId, Request),
    PowerOn,
    PowerOff,
}

pub struct ModemHandle {
    to_driver: Sender<ToDriver>,
    notices: Receiver<Notice>,
    next_id: u64,
}

type Pin = PinDriver<'static, AnyOutputPin, Output>;

impl ModemHandle {
    /// Open the UART and start the driver thread. The modem stays
    /// unpowered (and untouched) until `power_on`.
    pub fn start(
        uart: UART1,
        tx_pin: Gpio10,
        rx_pin: Gpio11,
        pwrkey: Gpio40,
        power_en: Gpio41,
    ) -> Result<Self, EspError> {
        let transport = UartTransport::start(uart, tx_pin, rx_pin, PIPE_CAPACITY)?;
        let mut pwrkey = PinDriver::output(pwrkey.downgrade_output())?;
        pwrkey.set_low()?;
        let power_en = PinDriver::output(power_en.downgrade_output())?;
        let (to_driver, from_handle) = mpsc::channel();
        let (to_handle, notices) = mpsc::channel();
        thread::Builder::new()
            .name("modem-driver".into())
            .stack_size(12 * 1024)
            .spawn(move || driver_loop(transport, pwrkey, power_en, from_handle, to_handle))
            .expect("spawn modem-driver thread");
        Ok(Self { to_driver, notices, next_id: 0 })
    }

    /// Power the modem on (POWER_EN, then a PWRKEY pulse). The HAL then
    /// syncs and configures; `Event::Configured` says when requests flow.
    pub fn power_on(&mut self) {
        self.send(ToDriver::PowerOn);
    }

    /// Power the modem off (PWRKEY off-pulse). Pending requests fail.
    pub fn power_off(&mut self) {
        self.send(ToDriver::PowerOff);
    }

    fn send(&self, m: ToDriver) {
        // The driver thread never exits, so this cannot fail in practice.
        self.to_driver.send(m).expect("modem driver thread is running");
    }
}

impl ModemPort for ModemHandle {
    fn submit(&mut self, request: Request) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        self.send(ToDriver::Submit(id, request));
        id
    }

    fn poll(&mut self) -> Option<Notice> {
        self.notices.try_recv().ok()
    }
}

fn driver_loop(
    mut t: UartTransport,
    mut pwrkey: Pin,
    mut power_en: Pin,
    from_handle: Receiver<ToDriver>,
    to_handle: Sender<Notice>,
) {
    let mut svc = Service::new();
    // HAL request id → handle request id.
    let mut ids: HashMap<RequestId, RequestId> = HashMap::new();
    let mut out = Vec::new();
    let mut set_pins = |pins: Vec<ServicePin>| {
        for p in pins {
            let result = match p {
                ServicePin::PowerEnable(on) => if on { power_en.set_high() } else { power_en.set_low() },
                // The board inverts PWRKEY: pressed = pin HIGH.
                ServicePin::PwrKey(pressed) => if pressed { pwrkey.set_high() } else { pwrkey.set_low() },
            };
            if let Err(e) = result {
                log::error!("modem: setting {p:?} failed: {e:?}");
            }
        }
    };
    loop {
        // In order: a power-off takes effect before a later request.
        while let Ok(m) = from_handle.try_recv() {
            match m {
                ToDriver::Submit(handle_id, request) => {
                    let hal_id = svc.submit(request, t.now());
                    ids.insert(hal_id, handle_id);
                }
                ToDriver::PowerOn => set_pins(svc.power_on(t.now())),
                ToDriver::PowerOff => set_pins(svc.power_off(t.now())),
            }
        }
        let (pins, result) = svc.step(&mut t, STEP_MS, &mut out);
        set_pins(pins);
        if let Err(e) = result {
            // The HAL will time the command out and resync; just record it.
            log::error!("modem: uart write failed: {e:?}");
        }
        for o in out.drain(..) {
            let notice = match o {
                hal::Output::Done { id, result } => {
                    let Some(id) = ids.remove(&id) else {
                        log::error!("modem: reply for unknown request {id:?}");
                        continue;
                    };
                    Notice::Done { id, result }
                }
                hal::Output::Event(e) => Notice::Event(e),
                hal::Output::Write(_) => unreachable!("driver::step carries out writes"),
            };
            // A dropped handle just means nobody is listening any more.
            let _ = to_handle.send(notice);
        }
    }
}
