//! Modem power sequencing (POWER_EN and the PWRKEY button) as a pure,
//! timed state machine, so the driver loop never sleeps through a pulse.
//!
//! A sequence is a list of steps, each a set of [`Action`]s carried out a
//! fixed time after the sequence starts. The driver calls
//! [`PowerSequencer::tick`] on every loop iteration and never waits past
//! [`PowerSequencer::next_deadline`]; in between it keeps serving the
//! modem link, so requests and notices keep flowing during a 3 s pulse.
//!
//! Timings (T-Deck-Pro, matching the captures):
//!   - on:  POWER_EN high → 100 ms → press PWRKEY → 1 s → release; the
//!     HAL is told the modem is on at release (the modem boots ~6 s later).
//!   - off: the HAL is told first (pending requests fail at once, nothing
//!     more is written), then press PWRKEY → 3 s → release.
//!
//! Invariants (callee):
//!   - Sequences run one at a time, in the order requested; a request
//!     made mid-sequence starts when the current one finishes.
//!   - Every press is followed by its release (no sequence is cut short).

use std::collections::VecDeque;

use super::link::Millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Drive the modem supply enable (GPIO41).
    PowerEnable(bool),
    /// Press (`true`) or release the PWRKEY button. The board inverts
    /// the pin: pressed = pin HIGH.
    PwrKey(bool),
    /// Tell the HAL the modem has been switched on.
    HalPowerOn,
    /// Tell the HAL the modem is being switched off.
    HalPowerOff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sequence {
    On,
    Off,
}

pub const ENABLE_SETTLE_MS: Millis = 100;
pub const PRESS_ON_MS: Millis = 1000;
pub const PRESS_OFF_MS: Millis = 3000;

/// `(offset from start, actions)`, offsets ascending.
fn steps(seq: Sequence) -> Vec<(Millis, Vec<Action>)> {
    use Action::*;
    match seq {
        Sequence::On => vec![
            (0, vec![PowerEnable(true)]),
            (ENABLE_SETTLE_MS, vec![PwrKey(true)]),
            (ENABLE_SETTLE_MS + PRESS_ON_MS, vec![PwrKey(false), HalPowerOn]),
        ],
        Sequence::Off => vec![(0, vec![HalPowerOff, PwrKey(true)]), (PRESS_OFF_MS, vec![PwrKey(false)])],
    }
}

struct Running {
    started: Millis,
    steps: VecDeque<(Millis, Vec<Action>)>,
}

#[derive(Default)]
pub struct PowerSequencer {
    running: Option<Running>,
    queued: VecDeque<Sequence>,
}

impl PowerSequencer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a sequence. Its first actions come out of the next `tick`.
    pub fn start(&mut self, seq: Sequence) {
        self.queued.push_back(seq);
    }

    /// The actions due by `now`, in order.
    pub fn tick(&mut self, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        loop {
            if self.running.is_none() {
                let Some(seq) = self.queued.pop_front() else { return out };
                self.running = Some(Running { started: now, steps: steps(seq).into() });
            }
            let r = self.running.as_mut().unwrap();
            while r.steps.front().is_some_and(|(at, _)| r.started + at <= now) {
                out.extend(r.steps.pop_front().unwrap().1);
            }
            if !r.steps.is_empty() {
                return out;
            }
            self.running = None; // finished: the next queued one may start now
        }
    }

    /// When `tick` must next be called, if a sequence is running or queued.
    pub fn next_deadline(&self) -> Option<Millis> {
        match &self.running {
            Some(r) => r.steps.front().map(|(at, _)| r.started + at),
            None if !self.queued.is_empty() => Some(0), // start right away
            None => None,
        }
    }

    pub fn is_busy(&self) -> bool {
        self.running.is_some() || !self.queued.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::*;

    #[test]
    fn idle_does_nothing() {
        let mut p = PowerSequencer::new();
        assert_eq!(p.tick(0), []);
        assert_eq!(p.next_deadline(), None);
        assert!(!p.is_busy());
    }

    #[test]
    fn power_on_steps_happen_on_time() {
        let mut p = PowerSequencer::new();
        p.start(Sequence::On);
        assert_eq!(p.tick(1000), [PowerEnable(true)]);
        assert_eq!(p.next_deadline(), Some(1100));
        assert_eq!(p.tick(1099), []);
        assert_eq!(p.tick(1100), [PwrKey(true)]);
        assert_eq!(p.next_deadline(), Some(2100));
        assert_eq!(p.tick(2100), [PwrKey(false), HalPowerOn]);
        assert!(!p.is_busy());
    }

    #[test]
    fn power_off_tells_the_hal_first() {
        let mut p = PowerSequencer::new();
        p.start(Sequence::Off);
        assert_eq!(p.tick(0), [HalPowerOff, PwrKey(true)]);
        assert_eq!(p.tick(2999), []);
        assert_eq!(p.tick(3000), [PwrKey(false)]);
    }

    #[test]
    fn late_tick_catches_up_in_order() {
        let mut p = PowerSequencer::new();
        p.start(Sequence::On);
        assert_eq!(p.tick(0), [PowerEnable(true)]);
        assert_eq!(p.tick(5000), [PwrKey(true), PwrKey(false), HalPowerOn]);
    }

    #[test]
    fn requests_mid_sequence_wait_their_turn() {
        let mut p = PowerSequencer::new();
        p.start(Sequence::On);
        p.tick(0);
        p.start(Sequence::Off);
        assert_eq!(p.tick(100), [PwrKey(true)]); // still the on-pulse
        assert_eq!(p.tick(1100), [PwrKey(false), HalPowerOn, HalPowerOff, PwrKey(true)]);
        assert_eq!(p.next_deadline(), Some(4100));
        assert_eq!(p.tick(4100), [PwrKey(false)]);
        assert!(!p.is_busy());
    }

    #[test]
    fn every_press_is_released() {
        for seqs in [vec![Sequence::On], vec![Sequence::Off], vec![Sequence::Off, Sequence::On, Sequence::Off]] {
            let mut p = PowerSequencer::new();
            seqs.iter().for_each(|&s| p.start(s));
            let mut actions = Vec::new();
            let mut now = 0;
            while p.is_busy() {
                actions.extend(p.tick(now));
                now = p.next_deadline().unwrap_or(now).max(now);
            }
            let presses = actions.iter().filter(|a| **a == PwrKey(true)).count();
            let releases = actions.iter().filter(|a| **a == PwrKey(false)).count();
            assert_eq!((presses, releases), (seqs.len(), seqs.len()), "{seqs:?}");
        }
    }
}
