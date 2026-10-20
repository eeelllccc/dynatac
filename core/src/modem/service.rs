//! The modem as the device runs it: the HAL plus power sequencing, behind
//! one ordered command interface. The device driver thread is a thin loop
//! around this; tests drive it against the simulator.
//!
//! Commands take effect in the order given. In particular a power
//! sequence's immediate steps happen when it is requested, so a request
//! submitted after `power_off` is refused (`NotSent`), never sent. (Found on
//! hardware: with the steps applied on the next loop pass instead, such a
//! request reached the modem and settled `Unknown`.)
//!
//! Pin changes come out as [`Pin`] actions for the caller to carry out;
//! everything else stays inside.

use super::driver;
use super::hal::{self, Hal, RequestId};
use super::link::Millis;
use super::power::{Action, PowerSequencer, Sequence};
use super::request::Request;
use super::transport::{Clock, Transport, WriteError};

/// A modem control pin change, for the caller to carry out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// Modem supply enable (GPIO41).
    PowerEnable(bool),
    /// PWRKEY pressed (`true`) or released. Pressed = pin HIGH on the board.
    PwrKey(bool),
}

#[derive(Default)]
pub struct Service {
    hal: Hal,
    power: PowerSequencer,
}

impl Service {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submit(&mut self, request: Request, now: Millis) -> RequestId {
        self.hal.submit(request, now)
    }

    /// Start the power-on sequence; returns the pin changes due now.
    pub fn power_on(&mut self, now: Millis) -> Vec<Pin> {
        self.power.start(Sequence::On);
        self.run_power(now)
    }

    /// Start the power-off sequence; returns the pin changes due now. The
    /// HAL stops at once: pending and later requests are refused.
    pub fn power_off(&mut self, now: Millis) -> Vec<Pin> {
        self.power.start(Sequence::Off);
        self.run_power(now)
    }

    /// One loop iteration: due power steps, then serve the link for at most
    /// `max_wait` (less if a power step is due sooner). HAL outputs other
    /// than writes go to `out`; pin changes are returned.
    pub fn step<T: Transport + Clock>(
        &mut self,
        t: &mut T,
        max_wait: Millis,
        out: &mut Vec<hal::Output>,
    ) -> (Vec<Pin>, Result<(), WriteError>) {
        let mut pins = self.run_power(t.now());
        let now = t.now();
        let wait = self.power.next_deadline().map_or(max_wait, |d| d.saturating_sub(now).min(max_wait));
        let result = driver::step(&mut self.hal, t, wait, out);
        pins.extend(self.run_power(t.now()));
        (pins, result)
    }

    pub fn is_ready(&self) -> bool {
        self.hal.is_ready()
    }

    fn run_power(&mut self, now: Millis) -> Vec<Pin> {
        let mut pins = Vec::new();
        for a in self.power.tick(now) {
            match a {
                Action::PowerEnable(on) => pins.push(Pin::PowerEnable(on)),
                Action::PwrKey(pressed) => pins.push(Pin::PwrKey(pressed)),
                Action::HalPowerOn => self.hal.power_on(now),
                Action::HalPowerOff => self.hal.power_off(now),
            }
        }
        pins
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::hal::{Event, Output, RequestError};
    use crate::modem::request::Response;
    use crate::modem::sim::{Sim, SimConfig, SimTransport};

    /// The simulator's power inputs driven by the pins, as on the board:
    /// a PWRKEY press switches the modem on (≥ 0.5 s, when off) or off
    /// (≥ 2.5 s, when on) at release; POWER_EN low cuts power.
    struct Board {
        svc: Service,
        t: SimTransport,
        out: Vec<Output>,
        pressed_at: Option<Millis>,
        modem_on: bool,
        pins: Vec<Pin>,
    }

    impl Board {
        fn new() -> Self {
            Board {
                svc: Service::new(),
                t: SimTransport::new(Sim::new(SimConfig::default())),
                out: vec![],
                pressed_at: None,
                modem_on: false,
                pins: vec![],
            }
        }

        fn apply(&mut self, pins: Vec<Pin>) {
            let now = self.t.now();
            for p in pins {
                self.pins.push(p);
                match p {
                    Pin::PowerEnable(false) => {
                        self.t.sim.cut_power(now);
                        self.modem_on = false;
                    }
                    Pin::PowerEnable(true) => {}
                    Pin::PwrKey(true) => self.pressed_at = Some(now),
                    Pin::PwrKey(false) => {
                        let held = now - self.pressed_at.take().expect("release after press");
                        if !self.modem_on && held >= 500 {
                            self.t.sim.power_on(now);
                            self.modem_on = true;
                        } else if self.modem_on && held >= 2500 {
                            self.t.sim.power_off(now);
                            self.modem_on = false;
                        }
                    }
                }
            }
        }

        fn power_on(&mut self) {
            let pins = self.svc.power_on(self.t.now());
            self.apply(pins);
        }

        fn power_off(&mut self) {
            let pins = self.svc.power_off(self.t.now());
            self.apply(pins);
        }

        fn submit(&mut self, r: Request) -> RequestId {
            self.svc.submit(r, self.t.now())
        }

        fn run(&mut self, ms: Millis) {
            let end = self.t.now() + ms;
            while self.t.now() < end {
                let (pins, r) = self.svc.step(&mut self.t, 20, &mut self.out);
                r.unwrap();
                self.apply(pins);
            }
        }

        fn done(&self, id: RequestId) -> Option<(Millis, &Result<Response, RequestError>)> {
            self.out.iter().find_map(|o| match o {
                Output::Done { id: i, result } if *i == id => Some((0, result)),
                _ => None,
            })
        }
    }

    #[test]
    fn power_on_sequence_boots_and_configures_the_modem() {
        let mut b = Board::new();
        b.power_on();
        b.run(30_000);
        assert_eq!(b.pins, [Pin::PowerEnable(true), Pin::PwrKey(true), Pin::PwrKey(false)]);
        assert!(b.svc.is_ready());
        assert!(b.out.contains(&Output::Event(Event::Configured { issues: vec![] })));
    }

    /// The hardware regression: power off, then submit straight away.
    #[test]
    fn request_after_power_off_is_refused_not_sent() {
        let mut b = Board::new();
        b.power_on();
        b.run(30_000);
        b.power_off();
        let id = b.submit(Request::ListCalls);
        b.run(50);
        assert_eq!(b.done(id).map(|(_, r)| r.clone()), Some(Err(RequestError::NotSent)), "refused at once");
        b.run(5000);
        assert!(
            !b.out.iter().any(|o| matches!(o, Output::Event(Event::Urc(crate::modem::unsolicited::Unsolicited::Unrecognised(_))))),
            "nothing was sent to the modem: {:#?}",
            b.out
        );
        assert_eq!(b.pins[3..], [Pin::PwrKey(true), Pin::PwrKey(false)]);
        assert!(!b.modem_on);
    }

    #[test]
    fn requests_are_served_while_pwrkey_is_held() {
        let mut b = Board::new();
        b.power_on();
        b.run(30_000);
        b.out.clear();
        // Power off with a request already queued ahead of it: the request
        // is settled immediately, not after the 3 s pulse.
        let id = b.submit(Request::QueryOutputGain);
        b.run(100);
        assert!(b.done(id).is_some(), "served within 100 ms");
        b.power_off();
        let start = b.t.now();
        b.run(3500);
        let release = b.t.now() - start;
        assert!(release >= 3000);
        assert!(b.out.contains(&Output::Event(Event::Urc(crate::modem::unsolicited::Unsolicited::PacketDomain(
            "ME DETACH".into()
        )))));
    }
}
