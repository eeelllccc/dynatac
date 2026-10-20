//! Contract tests for `ModemPort`: the mock (`MockHal`) must say what the
//! real HAL on the simulator says, so tests of services written against
//! the mock hold for the real stack.
//!
//! Each scenario runs on both and compares the notices, normalised:
//! packet-domain URCs (bearer up/down: network detail services ignore) are
//! dropped, call durations and request ids are blanked.

use super::{MockHal, SimModem};
use crate::modem::hal::Event;
use crate::modem::port::{ModemPort, Notice};
use crate::modem::request::{PhoneNumber, Request};
use crate::modem::sim::SimConfig;
use crate::modem::unsolicited::{CallInfo, CallMode, CallState, Direction, Unsolicited};

const NUMBER: &str = "07700900123";

fn drain(p: &mut impl ModemPort) -> Vec<String> {
    std::iter::from_fn(|| p.poll()).filter_map(normalise).collect()
}

fn normalise(n: Notice) -> Option<String> {
    match n {
        Notice::Event(Event::Urc(Unsolicited::PacketDomain(_))) => None,
        Notice::Event(Event::Urc(Unsolicited::VoiceCallEnd { duration_s })) => {
            Some(format!("VoiceCallEnd(answered: {})", duration_s.is_some()))
        }
        Notice::Done { result, .. } => Some(format!("Done({result:?})")),
        Notice::Event(e) => Some(format!("{e:?}")),
    }
}

/// A configured, fully booted simulated modem with nothing left to poll.
/// (Configuration finishes mid-boot: `+CPIN`, `SMS DONE` and `PB DONE`
/// arrive after `Configured`, as on the real modem.)
fn sim() -> SimModem {
    let mut m = SimModem::powered_on(SimConfig::default());
    m.run(15_000);
    drain(&mut m);
    m
}

/// An outgoing call, answered by the far end, on the simulator.
fn sim_in_call() -> SimModem {
    let mut m = sim();
    m.submit(Request::Dial(PhoneNumber::parse(NUMBER).unwrap()));
    m.run(4000);
    let now = m.now();
    m.t.sim.remote_answer(now);
    m.run(500);
    drain(&mut m);
    m
}

fn outgoing(state: CallState) -> CallInfo {
    CallInfo {
        index: 1,
        direction: Direction::Outgoing,
        state,
        mode: CallMode::Voice,
        multiparty: false,
        number: Some(NUMBER.into()),
    }
}

fn mock_in_call() -> MockHal {
    let mut f = MockHal::new();
    f.call_update(outgoing(CallState::Active));
    drain(&mut f);
    f
}

#[test]
fn dialling_sequence() {
    let mut m = sim();
    m.submit(Request::Dial(PhoneNumber::parse(NUMBER).unwrap()));
    m.run(4000);
    let now = m.now();
    m.t.sim.remote_answer(now);
    m.run(500);

    let mut f = MockHal::new();
    f.submit(Request::Dial(PhoneNumber::parse(NUMBER).unwrap()));
    f.call_update(outgoing(CallState::Dialling));
    f.call_update(outgoing(CallState::Alerting));
    f.remote_answer();

    assert_eq!(drain(&mut f), drain(&mut m));
}

#[test]
fn local_hang_up_events_come_before_the_response() {
    let mut m = sim_in_call();
    m.submit(Request::HangUp);
    m.run(2000);

    let mut f = mock_in_call();
    f.hold();
    f.submit(Request::HangUp);
    f.call_ended(Some(5));
    f.release_all();

    let sim_notices = drain(&mut m);
    assert_eq!(sim_notices.last().map(String::as_str), Some("Done(Ok(Done))"));
    assert_eq!(drain(&mut f), sim_notices);
}

#[test]
fn remote_hang_up_sequence() {
    let mut m = sim_in_call();
    let now = m.now();
    m.t.sim.remote_hang_up(now);
    m.run(1000);

    let mut f = mock_in_call();
    f.call_ended(Some(5));

    assert_eq!(drain(&mut f), drain(&mut m));
}

#[test]
fn incoming_call_first_ring() {
    let mut m = sim();
    let now = m.now();
    m.t.sim.incoming_call(NUMBER, now);
    m.run(100);

    let mut f = MockHal::new();
    f.incoming_call(NUMBER);

    assert_eq!(drain(&mut f), drain(&mut m));
}

#[test]
fn missed_call_sequence() {
    let mut m = sim();
    let now = m.now();
    m.t.sim.incoming_call(NUMBER, now);
    m.run(100);
    drain(&mut m);
    let now = m.now();
    m.t.sim.remote_hang_up(now);
    m.run(100);

    let mut f = MockHal::new();
    f.incoming_call(NUMBER);
    drain(&mut f);
    f.call_ended(None);

    assert_eq!(drain(&mut f), drain(&mut m));
}

#[test]
fn call_list_after_hang_up_is_empty_on_both() {
    let mut m = sim_in_call();
    m.submit(Request::HangUp);
    m.run(2000);
    m.submit(Request::ListCalls);
    m.run(500);

    let mut f = mock_in_call();
    f.call_ended(Some(5));
    f.submit(Request::HangUp);
    f.submit(Request::ListCalls);

    let last = |v: Vec<String>| v.last().cloned();
    assert_eq!(last(drain(&mut f)), last(drain(&mut m)));
}

/// Shared contract: one `Done` per request, in submission order.
fn submit_and_collect_dones(p: &mut impl ModemPort, settle: impl FnOnce(&mut dyn ModemPort)) {
    let requests = [Request::QueryOutputGain, Request::ListCalls, Request::HangUp, Request::Firmware];
    let ids: Vec<_> = requests.into_iter().map(|r| p.submit(r)).collect();
    settle(p);
    let dones: Vec<_> = std::iter::from_fn(|| p.poll())
        .filter_map(|n| if let Notice::Done { id, .. } = n { Some(id) } else { None })
        .collect();
    assert_eq!(dones, ids);
}

#[test]
fn both_keep_one_done_per_request_in_order() {
    submit_and_collect_dones(&mut MockHal::new(), |_| {});
    let mut m = sim();
    let requests = [Request::QueryOutputGain, Request::ListCalls, Request::HangUp, Request::Firmware];
    let ids: Vec<_> = requests.into_iter().map(|r| m.submit(r)).collect();
    m.run(2000);
    let dones: Vec<_> = std::iter::from_fn(|| m.poll())
        .filter_map(|n| if let Notice::Done { id, .. } = n { Some(id) } else { None })
        .collect();
    assert_eq!(dones, ids);
}
