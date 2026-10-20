//! [`SimModem`]: the real HAL driving the simulated modem, behind
//! [`ModemPort`]. Test builds only.

use std::collections::VecDeque;

use crate::modem::driver;
use crate::modem::hal::{self, Hal};
use crate::modem::link::Millis;
use crate::modem::port::{ModemPort, Notice};
use crate::modem::request::Request;
use crate::modem::hal::RequestId;
use crate::modem::sim::{Sim, SimConfig, SimTransport};
use crate::modem::transport::Clock;

/// The real HAL driving the simulated modem, on simulated time. Time only
/// passes in [`SimModem::run`]; `poll` returns what has happened so far.
pub(crate) struct SimModem {
    pub hal: Hal,
    pub t: SimTransport,
    pending: VecDeque<Notice>,
}

impl SimModem {
    /// Modem and HAL powered on together, run until configured (the
    /// `Configured` event is left to be polled).
    pub fn powered_on(cfg: SimConfig) -> Self {
        let mut m = SimModem { hal: Hal::new(), t: SimTransport::new(Sim::new(cfg)), pending: Default::default() };
        let now = m.t.now();
        m.t.sim.power_on(now);
        m.hal.power_on(now);
        while !m.hal.is_ready() {
            assert!(m.t.now() < 60_000, "simulated modem never configured");
            m.step();
        }
        m
    }

    pub fn now(&self) -> Millis {
        self.t.now()
    }

    /// Let `ms` of simulated time pass.
    pub fn run(&mut self, ms: Millis) {
        let end = self.t.now() + ms;
        while self.t.now() < end {
            self.step();
        }
    }

    fn step(&mut self) {
        let mut out = Vec::new();
        driver::step(&mut self.hal, &mut self.t, 50, &mut out).expect("sim writes never fail");
        self.pending.extend(out.into_iter().map(|o| match o {
            hal::Output::Done { id, result } => Notice::Done { id, result },
            hal::Output::Event(e) => Notice::Event(e),
            hal::Output::Write(_) => unreachable!("driver carries out writes"),
        }));
    }
}

impl ModemPort for SimModem {
    fn submit(&mut self, request: Request) -> RequestId {
        let now = self.t.now();
        self.hal.submit(request, now)
    }

    fn poll(&mut self) -> Option<Notice> {
        if self.pending.is_empty() {
            // Carry out the writes the submit produced, without letting time pass.
            let mut out = Vec::new();
            driver::flush(&mut self.hal, &mut self.t, &mut out).expect("sim writes never fail");
            self.pending.extend(out.into_iter().filter_map(|o| match o {
                hal::Output::Done { id, result } => Some(Notice::Done { id, result }),
                hal::Output::Event(e) => Some(Notice::Event(e)),
                hal::Output::Write(_) => None,
            }));
        }
        self.pending.pop_front()
    }
}
