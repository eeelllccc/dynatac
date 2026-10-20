//! `CallManager` against `MockHal` (exact control of replies, URC order and
//! lost URCs) and against `SimModem` (the real HAL on the simulator).

use super::*;
use crate::modem::link::{Failure, UnknownReason, UnsyncReason};
use crate::modem::port::tests::{MockHal, SimModem};
use crate::modem::sim::SimConfig;

const NUMBER: &str = "07700900123";

fn number() -> PhoneNumber {
    PhoneNumber::parse(NUMBER).unwrap()
}

fn call(direction: Direction, state: CallState) -> CallInfo {
    CallInfo { index: 1, direction, state, mode: CallMode::Voice, multiparty: false, number: Some(NUMBER.into()) }
}

fn outgoing(state: CallState) -> CallInfo {
    call(Direction::Outgoing, state)
}

fn incoming(state: CallState) -> CallInfo {
    call(Direction::Incoming, state)
}

fn ended(direction: Direction, connected_ms: Option<Millis>) -> CallStatus {
    CallStatus::Ended(Ended { direction, number: Some(NUMBER.into()), connected_ms })
}

fn list_calls_sent(f: &MockHal) -> usize {
    f.sent().iter().filter(|r| **r == Request::ListCalls).count()
}

/// A manager on a mock, with simulated time.
struct Rig {
    f: MockHal,
    cm: CallManager,
    now: Millis,
}

impl Rig {
    /// Configured and synced: the initial query has been answered (no calls).
    fn new() -> Self {
        let mut r = Rig { f: MockHal::new(), cm: CallManager::new(), now: 0 };
        r.f.emit(Event::Configured { issues: vec![] });
        r.tick();
        assert_eq!(r.cm.status(), CallStatus::Idle);
        r
    }

    /// Hand every pending notice to the manager (which may submit more).
    fn pump(&mut self) {
        while let Some(n) = self.f.poll() {
            self.cm.handle(&mut self.f, n, self.now);
        }
    }

    fn tick(&mut self) {
        self.cm.tick(&mut self.f, self.now);
        self.pump();
    }

    /// Let `ms` pass in 100 ms ticks.
    fn run(&mut self, ms: Millis) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 100;
            self.tick();
        }
    }

    fn status(&self) -> CallStatus {
        self.cm.status()
    }

    /// An outgoing call, connected at `self.now`.
    fn in_call() -> Self {
        let mut r = Rig::new();
        r.cm.dial(&mut r.f, number(), r.now).unwrap();
        r.f.call_update(outgoing(CallState::Dialling));
        r.f.remote_answer();
        r.pump();
        assert_eq!(r.status(), CallStatus::Active { number: Some(NUMBER.into()), since: 0 });
        r
    }

    /// Ringing in.
    fn ringing() -> Self {
        let mut r = Rig::new();
        r.f.incoming_call(NUMBER);
        r.pump();
        assert_eq!(r.status(), CallStatus::Incoming { number: Some(NUMBER.into()) });
        r
    }
}

// --- outgoing ---

#[test]
fn outgoing_call_lifecycle() {
    let mut r = Rig::new();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    assert_eq!(r.status(), CallStatus::Dialling { number: NUMBER.into() });
    r.f.call_update(outgoing(CallState::Dialling));
    r.pump();
    assert_eq!(r.status(), CallStatus::Dialling { number: NUMBER.into() });
    r.f.call_update(outgoing(CallState::Alerting));
    r.pump();
    assert_eq!(r.status(), CallStatus::Alerting { number: NUMBER.into() });

    r.now = 3_000;
    r.f.remote_answer();
    r.pump();
    assert_eq!(r.status(), CallStatus::Active { number: Some(NUMBER.into()), since: 3_000 });

    r.now = 38_000;
    r.f.call_ended(Some(35));
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, Some(35_000)));
    r.cm.dismiss();
    assert_eq!(r.status(), CallStatus::Idle);
}

/// The real modem sends `ATD`'s `OK` and the first `+CLCC` in either order.
#[test]
fn dial_ok_after_the_first_call_urc() {
    let mut r = Rig::new();
    r.f.hold();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.f.call_update(outgoing(CallState::Dialling));
    r.pump();
    r.f.release_all();
    r.pump();
    r.f.call_update(outgoing(CallState::Alerting));
    r.pump();
    assert_eq!(r.status(), CallStatus::Alerting { number: NUMBER.into() });
}

#[test]
fn dial_refused_by_the_modem_ends_at_once() {
    let mut r = Rig::new();
    r.f.on(|q| matches!(q, Request::Dial(_)), Err(RequestError::Failed(Failure::Cme("no network service".into()))));
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, None));
}

/// `OK`, but the call never shows up: the list taken after the `OK` decides.
#[test]
fn dial_accepted_but_no_call_ends_on_the_next_list() {
    let mut r = Rig::new();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, None));
}

/// A list taken *before* `ATD` was answered is not conclusive.
#[test]
fn list_from_before_the_dial_settled_does_not_end_it() {
    let mut r = Rig::new();
    r.f.hold();
    r.f.emit(Event::BytesLost); // a query goes out: its reply (empty) is held
    r.pump();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    assert!(r.f.release_one()); // the empty list, from before `ATD`
    r.pump();
    assert_eq!(r.status(), CallStatus::Dialling { number: NUMBER.into() });
    r.f.call_update(outgoing(CallState::Dialling));
    r.f.release_all(); // `ATD`'s `OK`
    r.pump();
    assert_eq!(r.status(), CallStatus::Dialling { number: NUMBER.into() });
    r.f.call_update(outgoing(CallState::Alerting));
    r.pump();
    assert_eq!(r.status(), CallStatus::Alerting { number: NUMBER.into() });
}

#[test]
fn dial_unknown_is_resolved_by_the_list() {
    let mut r = Rig::new();
    r.f.on(|q| matches!(q, Request::Dial(_)), Err(RequestError::Unknown(UnknownReason::Timeout)));
    // The call did start; its URCs were lost.
    r.f.set_calls(vec![outgoing(CallState::Alerting)]);
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.pump();
    assert_eq!(r.status(), CallStatus::Alerting { number: NUMBER.into() });
}

#[test]
fn cannot_dial_during_a_call() {
    let mut r = Rig::in_call();
    assert_eq!(r.cm.dial(&mut r.f, number(), r.now), Err(Refused::Busy));
    let mut r = Rig::ringing();
    assert_eq!(r.cm.dial(&mut r.f, number(), r.now), Err(Refused::Busy));
}

#[test]
fn hang_up_while_dialling_cancels() {
    let mut r = Rig::new();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.f.call_update(outgoing(CallState::Dialling));
    r.pump();
    r.f.hold();
    r.cm.hang_up(&mut r.f).unwrap();
    r.f.call_ended(None);
    r.f.release_all();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, None));
}

// --- incoming ---

#[test]
fn incoming_answered_then_hung_up_locally() {
    let mut r = Rig::ringing();
    r.now = 1_000;
    r.cm.answer(&mut r.f).unwrap();
    r.f.emit_urc(Unsolicited::VoiceCallBegin);
    r.f.call_update(incoming(CallState::Active));
    r.pump();
    assert_eq!(r.status(), CallStatus::Active { number: Some(NUMBER.into()), since: 1_000 });

    // The modem sends the call-ended URCs before `CHUP`'s `OK`.
    r.now = 11_000;
    r.f.hold();
    r.cm.hang_up(&mut r.f).unwrap();
    r.f.call_ended(Some(10));
    r.f.release_all();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Incoming, Some(10_000)));
}

#[test]
fn missed_call() {
    let mut r = Rig::ringing();
    r.f.call_ended(None);
    r.pump();
    assert_eq!(r.status(), ended(Direction::Incoming, None));
}

#[test]
fn rejected_call() {
    let mut r = Rig::ringing();
    r.f.hold();
    r.cm.hang_up(&mut r.f).unwrap();
    r.f.call_ended(None);
    r.f.release_all();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Incoming, None));
}

/// `ATA` after the caller gave up times out (`Unknown`): the list shows
/// there is no call.
#[test]
fn answer_too_late() {
    let mut r = Rig::ringing();
    r.f.on(|q| *q == Request::Answer, Err(RequestError::Unknown(UnknownReason::Timeout)));
    r.f.set_calls(vec![]); // ended, URCs lost
    r.cm.answer(&mut r.f).unwrap();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Incoming, None));
}

#[test]
fn answer_needs_an_incoming_call() {
    let mut r = Rig::new();
    assert_eq!(r.cm.answer(&mut r.f), Err(Refused::NoCall));
    let mut r = Rig::in_call();
    assert_eq!(r.cm.answer(&mut r.f), Err(Refused::NotIncoming));
    assert_eq!(Rig::new().cm.hang_up(&mut MockHal::new()), Err(Refused::NoCall));
}

/// A `RING` with no `+CLCC` (lost): the hint triggers a query.
#[test]
fn ring_alone_reveals_the_call_by_query() {
    let mut r = Rig::new();
    r.f.set_calls(vec![incoming(CallState::Incoming)]);
    r.f.emit_urc(Unsolicited::Ring);
    r.pump();
    assert_eq!(r.status(), CallStatus::Incoming { number: Some(NUMBER.into()) });
}

#[test]
fn a_new_call_replaces_the_ended_one() {
    let mut r = Rig::ringing();
    r.f.call_ended(None);
    r.pump();
    r.f.incoming_call(NUMBER);
    r.pump();
    assert_eq!(r.status(), CallStatus::Incoming { number: Some(NUMBER.into()) });
}

// --- reconciliation ---

/// Every state change's URC lost: the 2 s poll still follows the call.
#[test]
fn polling_follows_a_call_whose_urcs_were_lost() {
    let mut r = Rig::new();
    r.f.hold();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.f.call_update(outgoing(CallState::Dialling));
    r.f.release_all();
    r.pump();

    r.f.set_calls(vec![outgoing(CallState::Active)]);
    r.run(POLL_MS);
    assert!(matches!(r.status(), CallStatus::Active { .. }));
    let connected = r.now;

    r.f.set_calls(vec![]);
    r.run(POLL_MS + 5_000);
    let CallStatus::Ended(e) = r.status() else { panic!("{:?}", r.status()) };
    assert!(e.connected_ms.unwrap() <= POLL_MS + 100, "{e:?} from {connected}");
}

#[test]
fn polls_every_two_seconds_during_a_call_only() {
    let mut r = Rig::in_call();
    let before = list_calls_sent(&r.f);
    r.run(10 * POLL_MS);
    let polls = list_calls_sent(&r.f) - before;
    assert!((9..=11).contains(&polls), "{polls}");

    r.f.call_ended(Some(20));
    r.pump();
    let before = list_calls_sent(&r.f);
    r.run(10 * POLL_MS);
    assert_eq!(list_calls_sent(&r.f), before);
}

/// The first query fails: retried by the poll until it succeeds.
#[test]
fn failed_queries_are_retried() {
    let mut f = MockHal::new();
    f.on(|q| *q == Request::ListCalls, Err(RequestError::Failed(Failure::Error)));
    f.on(|q| *q == Request::ListCalls, Err(RequestError::NotSent));
    f.set_calls(vec![incoming(CallState::Incoming)]);
    let mut r = Rig { f, cm: CallManager::new(), now: 0 };
    r.tick();
    assert_eq!(r.status(), CallStatus::Idle);
    r.run(2 * POLL_MS);
    assert_eq!(r.status(), CallStatus::Incoming { number: Some(NUMBER.into()) });
}

/// A reply in flight when the call ends (here: listing the call as active)
/// must not bring the call back.
#[test]
fn stale_reply_does_not_resurrect_an_ended_call() {
    let mut r = Rig::in_call();
    r.f.hold();
    r.run(POLL_MS); // a poll is now held, its reply computed: call active
    assert!(r.cm.query.is_some());
    r.f.call_ended(Some(2));
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, Some(POLL_MS)));
    r.f.release_all();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, Some(POLL_MS)));
    assert!(r.cm.query.is_none());
}

#[test]
fn bytes_lost_triggers_a_query() {
    let mut r = Rig::in_call();
    r.f.set_calls(vec![]);
    r.f.emit(Event::BytesLost);
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, Some(0)));
}

#[test]
fn link_lost_mid_call_reconciles_after_resync() {
    let mut r = Rig::in_call();
    r.f.hold();
    r.cm.hang_up(&mut r.f).unwrap();
    r.f.link_lost(UnsyncReason::Timeout); // CHUP: Unknown
    r.f.set_calls(vec![]); // it did hang up
    r.f.release_all();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, Some(0)));
}

#[test]
fn restart_ends_the_call_at_once() {
    let mut r = Rig::in_call();
    r.now = 7_000;
    r.f.restart();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, Some(7_000)));
}

#[test]
fn restart_during_a_dial_ends_it() {
    let mut r = Rig::new();
    r.f.hold();
    r.cm.dial(&mut r.f, number(), r.now).unwrap();
    r.f.restart();
    r.f.release_all();
    r.pump();
    assert_eq!(r.status(), ended(Direction::Outgoing, None));
}

#[test]
fn non_voice_calls_are_ignored() {
    let mut r = Rig::new();
    let mut c = incoming(CallState::Incoming);
    c.mode = CallMode::Data;
    r.f.call_update(c);
    r.pump();
    r.run(POLL_MS);
    assert_eq!(r.status(), CallStatus::Idle);
}

/// Only `Done`s for its own requests are taken; others (another service's)
/// are ignored.
#[test]
fn foreign_responses_are_ignored() {
    let mut r = Rig::in_call();
    let id = r.f.submit(Request::ListCalls);
    r.f.set_calls(vec![]);
    r.cm.handle(&mut r.f, Notice::Done { id, result: Ok(Response::Calls(vec![])) }, r.now);
    assert!(matches!(r.status(), CallStatus::Active { .. }));
}

/// Enter pressed twice: one `ATA`. (With no call left, each extra `ATA`
/// would stall the link for its 10 s timeout.)
#[test]
fn repeated_actions_are_not_resent() {
    let mut r = Rig::ringing();
    r.f.hold();
    r.cm.answer(&mut r.f).unwrap();
    r.cm.answer(&mut r.f).unwrap();
    r.cm.hang_up(&mut r.f).unwrap();
    r.cm.hang_up(&mut r.f).unwrap();
    let count = |q: Request| r.f.sent().iter().filter(|s| **s == q).count();
    assert_eq!((count(Request::Answer), count(Request::HangUp)), (1, 1));
    r.f.release_all();
    r.pump();
    r.cm.hang_up(&mut r.f).unwrap(); // the first one is done: a new one goes out
    assert_eq!(r.f.sent().iter().filter(|s| **s == Request::HangUp).count(), 2);
}

// --- link readiness ---

#[test]
fn actions_wait_for_the_first_configured() {
    let mut f = MockHal::new();
    let mut cm = CallManager::new();
    assert_eq!(cm.dial(&mut f, number(), 0), Err(Refused::NotReady));
    assert!(f.sent().is_empty());
    cm.handle(&mut f, Notice::Event(Event::Configured { issues: vec![] }), 0);
    assert!(cm.is_ready());
    assert_eq!(cm.dial(&mut f, number(), 0), Ok(()));
}

/// Queued behind a resync, an `ATA` / `AT+CHUP` could run seconds later
/// against a different call: refused instead.
#[test]
fn actions_refused_while_the_link_is_down() {
    let mut r = Rig::ringing();
    r.f.hold();
    r.f.link_lost(UnsyncReason::Timeout); // emits LinkLost then Configured: take LinkLost alone
    let lost = r.f.poll().unwrap();
    r.cm.handle(&mut r.f, lost, r.now);
    let sent = r.f.sent().len();
    assert_eq!(r.cm.answer(&mut r.f), Err(Refused::NotReady));
    assert_eq!(r.cm.hang_up(&mut r.f), Err(Refused::NotReady));
    assert_eq!(r.f.sent().len(), sent);
    r.f.release_all();
    r.pump(); // Configured
    assert_eq!(r.cm.answer(&mut r.f), Ok(()));
}

#[test]
fn restart_takes_the_link_down_until_configured() {
    let mut r = Rig::new();
    r.f.restart(); // Restarted, Configured
    let restarted = r.f.poll().unwrap();
    r.cm.handle(&mut r.f, restarted, r.now);
    assert_eq!(r.cm.dial(&mut r.f, number(), r.now), Err(Refused::NotReady));
    r.pump();
    assert_eq!(r.cm.dial(&mut r.f, number(), r.now), Ok(()));
}

#[test]
fn volume_set_while_down_is_applied_on_configured() {
    let mut f = MockHal::new();
    let mut cm = CallManager::new();
    let low = Gain::new(2).unwrap();
    cm.set_volume(&mut f, low);
    assert!(f.sent().is_empty());
    cm.handle(&mut f, Notice::Event(Event::Configured { issues: vec![] }), 0);
    assert_eq!(f.sent()[0], Request::SetOutputGain(low));
}

// --- volume ---

#[test]
fn volume_is_reapplied_after_reconfiguration() {
    let mut r = Rig::in_call();
    let low = Gain::new(3).unwrap();
    r.cm.set_volume(&mut r.f, low);
    r.pump();
    assert_eq!(r.cm.volume(), low);
    r.f.restart(); // config resets the gain to max
    r.f.set_gain(Gain::MAX);
    r.pump();
    assert_eq!(r.f.sent().iter().filter(|q| **q == Request::SetOutputGain(low)).count(), 2);
}

#[test]
fn volume_at_max_is_not_resent() {
    let mut r = Rig::new();
    r.f.emit(Event::Configured { issues: vec![] });
    r.pump();
    assert!(!r.f.sent().iter().any(|q| matches!(q, Request::SetOutputGain(_))));
}

// --- the real HAL on the simulator ---

/// A manager on the simulated modem, booted and synced.
struct SimRig {
    m: SimModem,
    cm: CallManager,
}

impl SimRig {
    fn new() -> Self {
        let mut r = SimRig { m: SimModem::powered_on(SimConfig::default()), cm: CallManager::new() };
        r.run(15_000);
        assert_eq!(r.cm.status(), CallStatus::Idle);
        r
    }

    fn run(&mut self, ms: Millis) {
        let end = self.m.now() + ms;
        while self.m.now() < end {
            self.m.run(50);
            let now = self.m.now();
            while let Some(n) = self.m.poll() {
                self.cm.handle(&mut self.m, n, now);
            }
            self.cm.tick(&mut self.m, now);
        }
    }
}

#[test]
fn sim_outgoing_call_answered_and_hung_up() {
    let mut r = SimRig::new();
    let now = r.m.now();
    r.cm.dial(&mut r.m, number(), now).unwrap();
    r.run(4_000);
    assert!(matches!(r.cm.status(), CallStatus::Dialling { .. } | CallStatus::Alerting { .. }), "{:?}", r.cm.status());
    let now = r.m.now();
    r.m.t.sim.remote_answer(now);
    r.run(1_000);
    assert!(matches!(r.cm.status(), CallStatus::Active { .. }), "{:?}", r.cm.status());
    r.run(10_000);
    r.cm.hang_up(&mut r.m).unwrap();
    r.run(2_000);
    let CallStatus::Ended(e) = r.cm.status() else { panic!("{:?}", r.cm.status()) };
    assert_eq!(e.direction, Direction::Outgoing);
    assert!((10_000..12_000).contains(&e.connected_ms.unwrap()), "{e:?}");
    assert_eq!(r.m.t.sim.call_state(), None);
}

#[test]
fn sim_incoming_answered_then_remote_hang_up() {
    let mut r = SimRig::new();
    let now = r.m.now();
    r.m.t.sim.incoming_call(NUMBER, now);
    r.run(500);
    assert_eq!(r.cm.status(), CallStatus::Incoming { number: Some(NUMBER.into()) });
    r.cm.answer(&mut r.m).unwrap();
    r.run(1_000);
    assert!(matches!(r.cm.status(), CallStatus::Active { .. }), "{:?}", r.cm.status());
    let now = r.m.now();
    r.m.t.sim.remote_hang_up(now);
    r.run(1_000);
    assert!(matches!(r.cm.status(), CallStatus::Ended(Ended { direction: Direction::Incoming, connected_ms: Some(_), .. })));
}

#[test]
fn sim_missed_call() {
    let mut r = SimRig::new();
    let now = r.m.now();
    r.m.t.sim.incoming_call(NUMBER, now);
    r.run(13_000);
    assert!(matches!(r.cm.status(), CallStatus::Incoming { .. }));
    let now = r.m.now();
    r.m.t.sim.remote_hang_up(now);
    r.run(1_000);
    assert_eq!(r.cm.status(), ended(Direction::Incoming, None));
}

#[test]
fn sim_modem_power_loss_mid_call_ends_it_on_restart() {
    let mut r = SimRig::new();
    let now = r.m.now();
    r.cm.dial(&mut r.m, number(), now).unwrap();
    r.run(4_000);
    let now = r.m.now();
    r.m.t.sim.remote_answer(now);
    r.run(1_000);
    let now = r.m.now();
    r.m.t.sim.cut_power(now);
    r.m.t.sim.power_on(now);
    r.run(20_000);
    assert!(matches!(r.cm.status(), CallStatus::Ended(Ended { connected_ms: Some(_), .. })), "{:?}", r.cm.status());
}

// --- under faults: the real HAL over the faulty link ---

mod under_faults {
    use super::*;
    use crate::modem::driver;
    use crate::modem::faults::{Faults, FaultyLink, Rng, Split};
    use crate::modem::hal::{Hal, Output};
    use crate::modem::transport::Clock;

    /// The HAL as a port at one instant (responses arrive via the driver).
    struct At<'a>(&'a mut Hal, Millis);

    impl ModemPort for At<'_> {
        fn submit(&mut self, request: Request) -> RequestId {
            self.0.submit(request, self.1)
        }
        fn poll(&mut self) -> Option<Notice> {
            None
        }
    }

    struct Soak {
        hal: Hal,
        link: FaultyLink,
        cm: CallManager,
        rng: Rng,
        since: Option<(CallState, Millis)>,
        calls_ended: usize,
    }

    impl Soak {
        fn new(faults: Faults, seed: u64) -> Self {
            let mut s = Soak {
                hal: Hal::new(),
                link: FaultyLink::new(SimConfig::default(), faults, seed),
                cm: CallManager::new(),
                rng: Rng::new(seed ^ 0x5EED),
                since: None,
                calls_ended: 0,
            };
            s.link.sim.power_on(0);
            s.hal.power_on(0);
            s
        }

        fn step(&mut self) {
            let before = self.link.now();
            let mut out = Vec::new();
            driver::step(&mut self.hal, &mut self.link, 20, &mut out).unwrap();
            let now = self.link.now();
            self.link.roll_session_faults((now - before).max(1));
            let was_ended = matches!(self.cm.status(), CallStatus::Ended(_));
            for o in out {
                let n = match o {
                    Output::Done { id, result } => Notice::Done { id, result },
                    Output::Event(e) => Notice::Event(e),
                    Output::Write(_) => unreachable!("the driver carries out writes"),
                };
                self.cm.handle(&mut At(&mut self.hal, now), n, now);
            }
            self.cm.tick(&mut At(&mut self.hal, now), now);
            if !was_ended && matches!(self.cm.status(), CallStatus::Ended(_)) {
                self.calls_ended += 1;
            }
        }

        /// The user (through the manager) and the far end (on the sim).
        fn workload(&mut self) {
            let now = self.link.now();
            let state = self.link.sim.call_state();
            let age = match (state, self.since) {
                (Some(s), Some((p, t))) if s == p => now - t,
                (Some(s), _) => {
                    self.since = Some((s, now));
                    0
                }
                (None, _) => {
                    self.since = None;
                    0
                }
            };
            if !self.rng.chance(0.01) {
                return;
            }
            let mut port = At(&mut self.hal, now);
            match state {
                None => {
                    if self.rng.chance(0.5) {
                        let _ = self.cm.dial(&mut port, number(), now);
                    } else {
                        self.link.sim.incoming_call(NUMBER, now);
                    }
                }
                Some(CallState::Alerting) if age > 1000 => self.link.sim.remote_answer(now),
                Some(CallState::Incoming) if age > 1000 => {
                    if self.rng.chance(0.6) {
                        let _ = self.cm.answer(&mut port);
                    } else if self.rng.chance(0.5) {
                        let _ = self.cm.hang_up(&mut port);
                    } else {
                        self.link.sim.remote_hang_up(now);
                    }
                }
                Some(CallState::Active) if age > 3000 => {
                    if self.rng.chance(0.5) {
                        let _ = self.cm.hang_up(&mut port);
                    } else {
                        self.link.sim.remote_hang_up(now);
                    }
                }
                _ => {}
            }
        }

        /// Faults off, no workload, `ms` of quiet: the manager must agree
        /// with the simulated modem.
        fn check_converged(&mut self, ms: Millis) -> Result<(), String> {
            self.link.faults_on = false;
            let end = self.link.now() + ms;
            while self.link.now() < end {
                self.step();
            }
            self.link.faults_on = true;
            let (sim, cm) = (self.link.sim.call_state(), self.cm.status());
            let agree = match sim {
                None | Some(CallState::Disconnected) => matches!(cm, CallStatus::Idle | CallStatus::Ended(_)),
                Some(CallState::Dialling) => matches!(cm, CallStatus::Dialling { .. }),
                Some(CallState::Alerting) => matches!(cm, CallStatus::Alerting { .. }),
                Some(CallState::Active | CallState::Held) => matches!(cm, CallStatus::Active { .. }),
                Some(CallState::Incoming | CallState::Waiting) => matches!(cm, CallStatus::Incoming { .. }),
            };
            if agree {
                Ok(())
            } else {
                let recent: Vec<_> = self.link.traffic.iter().rev().take(40).collect();
                Err(format!("at {}: sim {sim:?}, manager {cm:?}\n{:#?}\nrecent traffic (newest first): {recent:#?}", self.link.now(), self.cm))
            }
        }

        /// Alternate faulty stretches with quiet checks.
        fn soak(&mut self, total: Millis) -> Result<(), String> {
            let end = self.link.now() + total;
            while self.link.now() < end {
                let stretch = self.link.now() + 20_000 + self.rng.below(40_000);
                while self.link.now() < stretch {
                    self.workload();
                    self.step();
                }
                // Long enough for a stalled command to time out, the link to
                // resync and a poll to land; and for a power cut's reboot.
                self.check_converged(30_000)?;
            }
            Ok(())
        }
    }

    const MIN: Millis = 60_000;

    #[test]
    fn clean_session_tracks_every_call() {
        let mut s = Soak::new(Faults::none(), 1);
        s.soak(20 * MIN).unwrap();
        assert!(s.calls_ended > 20, "workload too thin: {} calls", s.calls_ended);
    }

    /// Everything at once (as `faults::tests::randomised_fault_schedules`),
    /// many seeds: after every faulty stretch the manager converges on the
    /// modem's real call state.
    #[test]
    fn converges_after_randomised_faults() {
        let f = Faults {
            split: Split::Random,
            drop: 0.01,
            junk: 0.03,
            blank: 0.05,
            prompt: 0.01,
            urc: 0.05,
            stall: 0.005,
            restart: 0.002,
            pause: 0.01,
            corrupt: 0.0,
            queue: 512,
        };
        let mut ended = 0;
        for seed in 1..=40 {
            let mut s = Soak::new(f, seed);
            if let Err(e) = s.soak(15 * MIN) {
                panic!("seed {seed}: {e}");
            }
            ended += s.calls_ended;
        }
        assert!(ended > 200, "workload too thin: {ended} calls");
    }

    /// Heavy loss and restarts: the cases where URCs are certainly missed.
    #[test]
    fn converges_after_heavy_loss_and_restarts() {
        let f = Faults { split: Split::Random, drop: 0.05, pause: 0.05, restart: 0.01, queue: 256, ..Faults::none() };
        for seed in 100..120 {
            let mut s = Soak::new(f, seed);
            if let Err(e) = s.soak(15 * MIN) {
                panic!("seed {seed}: {e}");
            }
        }
    }
}
