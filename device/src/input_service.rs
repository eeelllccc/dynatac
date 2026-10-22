// The input task: sole owner of the shared I2C bus (GPIO13/14) and the
// devices on it that we use: keyboard (TCA8418), fuel gauge (BQ27220),
// charger (BQ25896). docs/os-design.md rule 1.
//
// Keys go to the kernel inbox as `Event::Key`. Battery and charger
// operations are requests to this task; `BatteryProxy` / `ChargerProxy`
// wrap them as the `BatteryDriver` / `ChargerDriver` the programs already
// use (blocking until the answer, as the programs block anyway).
//
// The task waits in exactly one place, its task notification, raised by
// the keyboard IRQ (GPIO15, active low, held while the TCA8418 FIFO has
// events) and by every request. The IRQ is level-triggered and armed
// before the line is re-read, so no key is left waiting.
//
// Caller invariants:
//   - One input task; it owns I2C0 and GPIO15.
//
// Callee invariants:
//   - Every request gets exactly one reply.
//   - Keys reach the kernel in the order pressed.

use std::num::NonZeroU32;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use esp_idf_svc::hal::delay::BLOCK;
use esp_idf_svc::hal::gpio::{Gpio15, IOPin, InterruptType, PinDriver, Pull};
use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::hal::task::notification::{Notification, Notifier};

use dynatac_core::battery::{BatteryDiag, BatteryDriver, BatteryError, ChargeState, BQ27220_ADDR};
use dynatac_core::charger::{ChargerDriver, ChargerError, BQ25896_ADDR};
use dynatac_core::kernel::Event;

use crate::battery::Battery;
use crate::charger::Charger;
use crate::i2c_bus::I2cBus;
use crate::keyboard::Keyboard;

const KEYBOARD_ADDR: u8 = 0x34;
const STACK_BYTES: usize = 8 * 1024;

enum Req {
    Level(Sender<Result<u8, BatteryError>>),
    ChargeState(Sender<Result<ChargeState, BatteryError>>),
    Diag(Sender<Result<BatteryDiag, BatteryError>>),
    Provision(Sender<Result<(), BatteryError>>),
    Shutdown(Sender<Result<(), ChargerError>>),
}

#[derive(Clone)]
struct Link {
    to_task: Sender<Req>,
    notifier: Arc<Notifier>,
}

impl Link {
    fn ask<T>(&self, make: impl FnOnce(Sender<T>) -> Req) -> Option<T> {
        let (tx, rx) = mpsc::channel();
        self.to_task.send(make(tx)).ok()?;
        // Safety: the notification lives as long as the task, whose loop
        // never exits.
        unsafe { self.notifier.notify(NonZeroU32::MIN) };
        rx.recv().ok()
    }
}

pub struct BatteryProxy(Link);
pub struct ChargerProxy(Link);

impl BatteryDriver for BatteryProxy {
    fn level(&mut self) -> Result<u8, BatteryError> {
        self.0.ask(Req::Level).unwrap_or(Err(BatteryError::Bus))
    }
    fn charge_state(&mut self) -> Result<ChargeState, BatteryError> {
        self.0.ask(Req::ChargeState).unwrap_or(Err(BatteryError::Bus))
    }
    fn diag(&mut self) -> Result<BatteryDiag, BatteryError> {
        self.0.ask(Req::Diag).unwrap_or(Err(BatteryError::Bus))
    }
    fn provision(&mut self) -> Result<(), BatteryError> {
        self.0.ask(Req::Provision).unwrap_or(Err(BatteryError::Bus))
    }
}

impl ChargerDriver for ChargerProxy {
    fn shutdown(&mut self) -> Result<(), ChargerError> {
        self.0.ask(Req::Shutdown).unwrap_or(Err(ChargerError::Bus))
    }
}

/// Start the input task. Returns once the devices are set up.
pub fn start(i2c: I2cDriver<'static>, irq: Gpio15, to_kernel: Sender<Event>) -> (BatteryProxy, ChargerProxy) {
    let (to_task, requests) = mpsc::channel();
    let (notifier_tx, notifier_rx) = mpsc::channel();
    thread::Builder::new()
        .name("input".into())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let notification = Notification::new();
            notifier_tx.send(notification.notifier()).expect("start() waits for this");
            task(i2c, irq, notification, requests, to_kernel)
        })
        .expect("spawn input thread");
    let notifier = notifier_rx.recv().expect("input task sends its notifier");
    let link = Link { to_task, notifier };
    (BatteryProxy(link.clone()), ChargerProxy(link))
}

fn task(i2c: I2cDriver<'static>, irq: Gpio15, notification: Notification, requests: Receiver<Req>, to_kernel: Sender<Event>) {
    let bus = I2cBus::new(i2c);
    let mut charger = Charger::new(bus.device(BQ25896_ADDR));
    let mut battery = Battery::new(bus.device(BQ27220_ADDR));
    // The BQ25896 watchdog first: while it runs, the chip reverts our
    // register changes every ~40 s.
    if let Err(e) = charger.disable_watchdog() {
        log::warn!("input: can't disable the BQ25896 watchdog: {e:?}");
    }
    // So that holding the power button doesn't reset the PMIC.
    if let Err(e) = charger.disable_long_press_reset() {
        log::warn!("input: can't disable the BQ25896 long-press reset: {e:?}");
    }
    let mut kb = match Keyboard::new(bus.device(KEYBOARD_ADDR)) {
        Ok(kb) => Some(kb),
        Err(e) => {
            log::error!("input: keyboard init failed: {e:?}");
            None
        }
    };
    let mut irq = PinDriver::input(irq.downgrade()).expect("GPIO15 as input");
    irq.set_pull(Pull::Up).expect("GPIO15 pull-up");
    let notifier = notification.notifier();
    // Safety: runs in ISR context and only notifies a task; the
    // notification outlives the subscription (the loop never exits).
    unsafe {
        irq.subscribe(move || {
            notifier.notify_and_yield(NonZeroU32::MIN);
        })
    }
    .expect("subscribe to the keyboard IRQ");
    irq.set_interrupt_type(InterruptType::LowLevel).expect("level interrupt");

    loop {
        if let Some(kb) = kb.as_mut() {
            loop {
                match kb.poll() {
                    Ok(Some(key)) => {
                        // A dropped kernel means nobody is listening.
                        let _ = to_kernel.send(Event::Key(key));
                    }
                    Ok(None) => break,
                    Err(e) => {
                        log::error!("input: keyboard read failed: {e:?}");
                        break;
                    }
                }
            }
        }
        while let Ok(req) = requests.try_recv() {
            // A dropped reply channel means the asker gave up: fine.
            match req {
                Req::Level(r) => drop(r.send(battery.level())),
                Req::ChargeState(r) => drop(r.send(battery.charge_state())),
                Req::Diag(r) => drop(r.send(battery.diag())),
                Req::Provision(r) => drop(r.send(battery.provision())),
                Req::Shutdown(r) => drop(r.send(charger.shutdown())),
            }
        }
        // Without a keyboard the line may sit low: don't arm a level
        // interrupt that would fire forever.
        if kb.is_some() {
            if let Err(e) = irq.enable_interrupt() {
                log::error!("input: can't arm the keyboard IRQ: {e:?}");
            }
            if irq.is_low() {
                continue; // more keys already waiting
            }
        }
        notification.wait(BLOCK);
    }
}
