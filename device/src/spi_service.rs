// The spi task: the only code that touches SPI2 and its devices' pins
// (docs/os-design.md rule 1). A thin loop around `dynatac_core::spi::
// scheduler::Spi`, which the core tests drive against `MockWire`.
//
// The task waits in exactly one place: its FreeRTOS task notification,
// until the scheduler's next deadline. Requests from the handle and GPIO
// interrupts on the pins the scheduler waits on both raise it.
//
// Lost wake-ups: a pin interrupt is level-triggered on the awaited level
// and armed *before* the pin is re-read; if the level is already there the
// task steps again instead of sleeping. So a level reached before arming
// is still seen.
//
// Responses go to the kernel inbox as `Event::Spi`.
//
// Caller invariants:
//   - One `SpiHandle`; it owns SPI2 and the bus pins.
//
// Callee invariants (from `Spi`):
//   - Every submitted request gets exactly one response, with its id.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use esp_idf_svc::hal::delay::{TickType, BLOCK};
use esp_idf_svc::hal::gpio::InterruptType;
use esp_idf_svc::hal::spi::SPI2;
use esp_idf_svc::hal::task::notification::{Notification, Notifier};
use esp_idf_svc::sys::EspError;

use dynatac_core::kernel::{Event, SpiPort};
use dynatac_core::spi::scheduler::{Request, RequestId, Spi};
use dynatac_core::spi::wire::{Clock, Input};

use crate::spi_wire::{EspWire, Pins};

const STACK_BYTES: usize = 8 * 1024;
const ALL_INPUTS: [Input; 3] = [Input::EpdBusy, Input::LoraBusy, Input::LoraDio1];

pub struct SpiHandle {
    to_task: Sender<(RequestId, Request)>,
    notifier: Arc<Notifier>,
    next_id: u64,
}

impl SpiHandle {
    /// Start the spi task. The bus is brought up on its first step.
    pub fn start(spi: SPI2, pins: Pins, to_kernel: Sender<Event>) -> Result<Self, EspError> {
        let wire = EspWire::new(spi, pins)?;
        let (to_task, requests) = mpsc::channel();
        let (notifier_tx, notifier_rx) = mpsc::channel();
        thread::Builder::new()
            .name("spi".into())
            .stack_size(STACK_BYTES)
            .spawn(move || {
                // The notification belongs to the task that waits on it.
                let notification = Notification::new();
                notifier_tx.send(notification.notifier()).expect("start() waits for this");
                task_loop(wire, notification, requests, to_kernel)
            })
            .expect("spawn spi thread");
        let notifier = notifier_rx.recv().expect("spi task sends its notifier");
        Ok(SpiHandle { to_task, notifier, next_id: 0 })
    }
}

impl SpiPort for SpiHandle {
    fn submit(&mut self, request: Request) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        self.to_task.send((id, request)).expect("spi task is running");
        // Safety: the notification lives as long as the task, which never
        // ends (its loop has no exit).
        unsafe { self.notifier.notify(NonZeroU32::MIN) };
        id
    }
}

fn task_loop(
    mut wire: EspWire,
    notification: Notification,
    requests: Receiver<(RequestId, Request)>,
    to_kernel: Sender<Event>,
) {
    for pin in ALL_INPUTS {
        let notifier = notification.notifier();
        // Safety: runs in ISR context and only notifies a task (an allowed
        // FreeRTOS call); the notification outlives the subscription.
        let subscribed = unsafe {
            wire.input_mut(pin).subscribe(move || {
                notifier.notify_and_yield(NonZeroU32::MIN);
            })
        };
        if let Err(e) = subscribed {
            log::error!("spi: can't subscribe to {pin:?}: {e:?}");
        }
    }
    let mut spi = Spi::new();
    // Scheduler id → handle id.
    let mut ids = HashMap::new();
    let mut out = Vec::new();
    loop {
        while let Ok((handle_id, request)) = requests.try_recv() {
            ids.insert(spi.submit(request), handle_id);
        }
        spi.step(&mut wire, &mut out);
        for (id, response) in out.drain(..) {
            let id = ids.remove(&id).expect("a response for every request");
            // A dropped kernel just means nobody is listening any more.
            let _ = to_kernel.send(Event::Spi(id, response));
        }
        let now = wire.now();
        let wake = spi.wake(now);
        if arm(&mut wire, &wake.pins) {
            continue; // a pin is already at its level
        }
        let timeout = wake.deadline.map_or(BLOCK, |d| TickType::new_millis(d.saturating_sub(now)).ticks());
        notification.wait(timeout);
    }
}

/// Arm a level interrupt for each awaited pin, then re-read them. True if
/// one is already at its level (don't sleep).
fn arm(wire: &mut EspWire, pins: &[(Input, bool)]) -> bool {
    let mut ready = false;
    for &(pin, high) in pins {
        let d = wire.input_mut(pin);
        let level = if high { InterruptType::HighLevel } else { InterruptType::LowLevel };
        if let Err(e) = d.set_interrupt_type(level).and_then(|_| d.enable_interrupt()) {
            log::error!("spi: can't arm {pin:?}: {e:?}");
        }
        ready |= d.is_high() == high;
    }
    ready
}
