//! The spi task's logic: requests in, one transaction at a time on the
//! bus, round-robin across the devices, responses out.
//!
//! [`Spi`] owns the [`Bus`] and one protocol per device. Each `step`
//! brings the bus up if needed, then repeatedly: resolves every device's
//! waits (time passed, pin level reached, deadline passed) and runs one
//! transaction for the next device in turn that has one ready. It stops
//! when no device has a transaction ready, and [`Spi::wake`] says when to
//! step again.
//!
//! Callee invariants:
//!   - S1 Round-robin: a device with a transaction ready runs before any
//!     other device runs two. So it waits at most one transaction per
//!     other device: the largest is an e-paper chunk (≈ 6.5 ms), so ≈ 7 ms
//!     with today's devices (`docs/spi-design.md`).
//!   - Every request gets exactly one response, with its id, in request
//!     order per device.
//!   - Waits are resolved every time round, so a pin reaching its level
//!     is acted on before the next transaction of any other device.
//!
//! Caller invariants (the spi task):
//!   - Call `step` when a request arrives, when a pin in `wake().pins`
//!     reaches its level, and at `wake().deadline`. The wire's clock never
//!     goes back.
//!   - One `Spi` per bus; nothing else drives the wire.

use std::collections::VecDeque;

use super::bus::{Bus, Device};
use super::epaper::{self, Epaper};
use super::protocol::{Next, Outcome};
use super::wire::{Clock, Input, Wire};
use super::Millis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Epaper(epaper::Request),
}

impl Request {
    pub fn device(&self) -> Device {
        match self {
            Request::Epaper(_) => Device::Epaper,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Epaper(epaper::Done),
    /// No protocol for the request's device (e.g. SD or LoRa, not yet built).
    NoDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequestId(pub u64);

/// When the spi task should call `step` again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Wake {
    pub deadline: Option<Millis>,
    /// Pins being waited on, with the level awaited.
    pub pins: Vec<(Input, bool)>,
}

/// A device protocol as the scheduler drives it (see
/// [`protocol`](super::protocol) for the contract).
pub trait DeviceProtocol {
    /// Queue a request for this device. Only given requests whose
    /// [`Request::device`] is this protocol's.
    fn submit(&mut self, request: Request);
    fn next(&mut self, now: Millis) -> Next;
    fn advance(&mut self, outcome: Outcome, now: Millis);
    /// The next finished request's response, in request order.
    fn take_done(&mut self) -> Option<Response>;
}

impl DeviceProtocol for Epaper {
    fn submit(&mut self, request: Request) {
        let Request::Epaper(r) = request;
        Epaper::submit(self, r);
    }
    fn next(&mut self, now: Millis) -> Next {
        Epaper::next(self, now)
    }
    fn advance(&mut self, outcome: Outcome, now: Millis) {
        Epaper::advance(self, outcome, now)
    }
    fn take_done(&mut self) -> Option<Response> {
        Epaper::take_done(self).map(Response::Epaper)
    }
}

/// How long to wait before retrying a failed bus bring-up.
pub const BRING_UP_RETRY_MS: Millis = 100;

struct Slot {
    device: Device,
    protocol: Box<dyn DeviceProtocol>,
    /// Ids of submitted requests not yet answered, oldest first.
    ids: VecDeque<RequestId>,
}

pub struct Spi {
    bus: Bus,
    slots: Vec<Slot>,
    /// Index of the slot whose turn is next.
    turn: usize,
    next_id: u64,
    /// When to retry bring-up after a failure.
    retry_at: Option<Millis>,
    /// Responses for requests no protocol takes, sent on the next step.
    orphans: Vec<(RequestId, Response)>,
}

impl Spi {
    /// The device set as built today: the e-paper.
    pub fn new() -> Self {
        Self::with_devices(vec![(Device::Epaper, Box::new(Epaper::new()))])
    }

    /// Any device set (tests add mock protocols). One protocol per device.
    pub fn with_devices(devices: Vec<(Device, Box<dyn DeviceProtocol>)>) -> Self {
        let slots = devices.into_iter().map(|(device, protocol)| Slot { device, protocol, ids: VecDeque::new() }).collect();
        Spi { bus: Bus::new(), slots, turn: 0, next_id: 0, retry_at: None, orphans: Vec::new() }
    }

    pub fn submit(&mut self, request: Request) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        match self.slots.iter_mut().find(|s| s.device == request.device()) {
            Some(slot) => {
                slot.ids.push_back(id);
                slot.protocol.submit(request);
            }
            None => self.orphans.push((id, Response::NoDevice)),
        }
        id
    }

    /// Run every transaction that is ready, round-robin, until none is.
    /// The clock is read afresh each time round: a wait expiring while
    /// another device uses the bus is seen at once.
    pub fn step<W: Wire + Clock>(&mut self, wire: &mut W, out: &mut Vec<(RequestId, Response)>) {
        out.append(&mut self.orphans);
        let now = wire.now();
        if !self.bus.is_up() {
            if self.retry_at.is_some_and(|t| now < t) {
                return;
            }
            if let Err(e) = self.bus.bring_up(wire) {
                log::error!("spi: bus bring-up failed: {e:?}");
                self.retry_at = Some(now + BRING_UP_RETRY_MS);
                return;
            }
            self.retry_at = None;
        }
        loop {
            let n = self.slots.len();
            let mut ran = false;
            for k in 0..n {
                let i = (self.turn + k) % n;
                if let Some(t) = self.ready(i, wire, out) {
                    let result = self.bus.run(wire, &t);
                    let now = wire.now();
                    self.slots[i].protocol.advance(Outcome::Tx(result), now);
                    self.collect(i, out);
                    self.turn = (i + 1) % n;
                    ran = true;
                    break;
                }
            }
            if !ran {
                return;
            }
        }
    }

    /// Resolve slot `i`'s waits; its transaction if one is ready now.
    fn ready<W: Wire + Clock>(
        &mut self,
        i: usize,
        wire: &mut W,
        out: &mut Vec<(RequestId, Response)>,
    ) -> Option<super::bus::Transaction> {
        loop {
            let now = wire.now();
            let next = self.slots[i].protocol.next(now);
            self.collect(i, out);
            let outcome = match next {
                Next::Idle => return None,
                Next::Transaction(t) => return Some(t),
                Next::WaitUntil(t) if now >= t => Outcome::Waited,
                Next::WaitUntil(_) => return None,
                Next::WaitPin { pin, high, deadline } => {
                    if self.bus.read(wire, pin) == high {
                        Outcome::Waited
                    } else if now >= deadline {
                        Outcome::TimedOut
                    } else {
                        return None;
                    }
                }
            };
            self.slots[i].protocol.advance(outcome, now);
        }
    }

    fn collect(&mut self, i: usize, out: &mut Vec<(RequestId, Response)>) {
        let slot = &mut self.slots[i];
        while let Some(response) = slot.protocol.take_done() {
            let id = slot.ids.pop_front().expect("a response for every request, no more");
            out.push((id, response));
        }
    }

    /// When to step next: the earliest deadline, and the pins awaited.
    /// Call after `step`.
    pub fn wake(&mut self, now: Millis) -> Wake {
        let mut wake = Wake::default();
        if !self.orphans.is_empty() {
            wake.deadline = Some(now);
            return wake;
        }
        if !self.bus.is_up() {
            wake.deadline = Some(self.retry_at.unwrap_or(now));
            return wake;
        }
        let earliest = |d: &mut Option<Millis>, t: Millis| *d = Some(d.map_or(t, |x| x.min(t)));
        for slot in &mut self.slots {
            match slot.protocol.next(now) {
                Next::Idle => {}
                // `step` leaves no transaction ready; if one is, step now.
                Next::Transaction(_) => earliest(&mut wake.deadline, now),
                Next::WaitUntil(t) => earliest(&mut wake.deadline, t),
                Next::WaitPin { pin, high, deadline } => {
                    earliest(&mut wake.deadline, deadline);
                    wake.pins.push((pin, high));
                }
            }
        }
        wake
    }
}

impl Default for Spi {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
