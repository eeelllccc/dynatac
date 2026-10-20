//! The whole modem stack (L0 transport → L1 link → L2 HAL, via the shared
//! driver loop) against the simulated A7682E.

use crate::modem::driver;
use crate::modem::hal::{Event, Hal, Output, RequestError, RequestId};
use crate::modem::link::{Millis, UnknownReason, UnsyncReason};
use crate::modem::request::{Gain, PhoneNumber, Request, Response};
use crate::modem::sim::{Sim, SimConfig, SimTransport};
use crate::modem::transport::Clock;
use crate::modem::unsolicited::{CallState, Unsolicited};

struct Stack {
    hal: Hal,
    t: SimTransport,
    out: Vec<Output>,
}

impl Stack {
    /// Modem and HAL powered on together, run until configured.
    fn configured() -> Self {
        let mut s = Stack { hal: Hal::new(), t: SimTransport::new(Sim::new(SimConfig::default())), out: vec![] };
        s.power_on();
        s.until(30_000, |o| matches!(o, Output::Event(Event::Configured { .. })));
        assert!(s.out.contains(&Output::Event(Event::Configured { issues: vec![] })), "{:#?}", s.out);
        s.out.clear();
        s
    }

    fn power_on(&mut self) {
        let now = self.t.now();
        self.t.sim.power_on(now);
        self.hal.power_on(now);
    }

    fn submit(&mut self, r: Request) -> RequestId {
        self.hal.submit(r, self.t.now())
    }

    /// Run the stack until an output matches (returns it) or `limit_ms` of
    /// simulated time passes (panics).
    fn until(&mut self, limit_ms: Millis, pred: impl Fn(&Output) -> bool) -> Output {
        let deadline = self.t.now() + limit_ms;
        let mut seen = self.out.len();
        while self.t.now() < deadline {
            driver::step(&mut self.hal, &mut self.t, 50, &mut self.out).unwrap();
            if let Some(o) = self.out[seen..].iter().find(|o| pred(o)) {
                return o.clone();
            }
            seen = self.out.len();
        }
        panic!("timed out; outputs so far: {:#?}", self.out);
    }

    fn done(&mut self, id: RequestId, limit_ms: Millis) -> Result<Response, RequestError> {
        match self.until(limit_ms, |o| matches!(o, Output::Done { id: i, .. } if *i == id)) {
            Output::Done { result, .. } => result,
            _ => unreachable!(),
        }
    }

    fn run(&mut self, ms: Millis) {
        let end = self.t.now() + ms;
        while self.t.now() < end {
            driver::step(&mut self.hal, &mut self.t, 50, &mut self.out).unwrap();
        }
    }

    fn call_states(&self) -> Vec<CallState> {
        self.out
            .iter()
            .filter_map(|o| match o {
                Output::Event(Event::Urc(Unsolicited::Call(c))) => Some(c.state),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn configures_the_modem_after_power_on() {
    let s = Stack::configured();
    assert_eq!(s.t.sim.gain(), 7);
}

#[test]
fn outgoing_call_answered_and_hung_up() {
    let mut s = Stack::configured();
    let dial = s.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
    assert_eq!(s.done(dial, 25_000), Ok(Response::Done));
    s.run(4000);
    let now = s.t.now();
    s.t.sim.remote_answer(now);
    s.run(5000);
    let hang_up = s.submit(Request::HangUp);
    assert_eq!(s.done(hang_up, 10_000), Ok(Response::Done));
    assert_eq!(
        s.call_states(),
        [CallState::Dialling, CallState::Alerting, CallState::Active, CallState::Disconnected]
    );
    assert!(s.out.contains(&Output::Event(Event::Urc(Unsolicited::NoCarrier))));
    let list = s.submit(Request::ListCalls);
    assert_eq!(s.done(list, 10_000), Ok(Response::Calls(vec![])));
}

#[test]
fn incoming_call_with_caller_id_answered() {
    let mut s = Stack::configured();
    let now = s.t.now();
    s.t.sim.incoming_call("07700900123", now);
    let id = s.until(1000, |o| matches!(o, Output::Event(Event::Urc(Unsolicited::CallerId { .. }))));
    assert_eq!(id, Output::Event(Event::Urc(Unsolicited::CallerId { number: Some("07700900123".into()) })));
    let answer = s.submit(Request::Answer);
    assert_eq!(s.done(answer, 25_000), Ok(Response::Done));
    assert_eq!(s.t.sim.call_state(), Some(CallState::Active));
    assert_eq!(s.call_states(), [CallState::Incoming, CallState::Active]);
}

#[test]
fn hang_up_works_because_the_hal_configured_voice_hang_up() {
    // ATH only works for voice calls after AT+CVHU=0, which the HAL applies.
    let mut s = Stack::configured();
    let dial = s.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
    s.done(dial, 25_000).unwrap();
    s.run(4000);
    let now = s.t.now();
    s.t.sim.remote_answer(now);
    s.run(500);
    let now = s.t.now();
    s.t.sim.write(b"ATH\r", now); // as an old code path would
    s.run(2000);
    assert_eq!(s.t.sim.call_state(), None);
}

#[test]
fn hard_power_cut_mid_request_recovers_and_reconfigures() {
    let mut s = Stack::configured();
    let now = s.t.now();
    s.t.sim.cut_power(now);
    let list = s.submit(Request::ListCalls);
    assert_eq!(s.done(list, 10_000), Err(RequestError::Unknown(UnknownReason::Timeout)));
    assert!(s.out.contains(&Output::Event(Event::LinkLost(UnsyncReason::Timeout))));
    let queued = s.submit(Request::QueryOutputGain);
    s.run(5000); // modem dead: probes go unanswered, request waits
    assert!(!s.out.iter().any(|o| matches!(o, Output::Done { id, .. } if *id == queued)));
    let now = s.t.now();
    s.t.sim.power_on(now); // power restored
    s.until(30_000, |o| matches!(o, Output::Event(Event::Restarted)));
    s.until(30_000, |o| matches!(o, Output::Event(Event::Configured { .. })));
    assert_eq!(s.done(queued, 10_000), Ok(Response::Gain(Gain::MAX)));
}

#[test]
fn modem_restart_mid_call_is_detected_and_settings_restored() {
    let mut s = Stack::configured();
    let dial = s.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
    s.done(dial, 25_000).unwrap();
    let now = s.t.now();
    s.t.sim.cut_power(now);
    s.t.sleep(3000);
    let now = s.t.now();
    s.t.sim.power_on(now);
    s.until(30_000, |o| matches!(o, Output::Event(Event::Restarted)));
    s.until(30_000, |o| matches!(o, Output::Event(Event::Configured { .. })));
    let list = s.submit(Request::ListCalls);
    assert_eq!(s.done(list, 10_000), Ok(Response::Calls(vec![])), "the call died with the modem");
}

#[test]
fn power_off_is_clean() {
    let mut s = Stack::configured();
    let now = s.t.now();
    s.hal.power_off(now);
    s.t.sim.power_off(now);
    let r = s.submit(Request::ListCalls);
    s.run(1000);
    assert!(s.out.contains(&Output::Done { id: r, result: Err(RequestError::NotSent) }));
    assert!(s.out.contains(&Output::Event(Event::Urc(Unsolicited::PacketDomain("ME DETACH".into())))));
}
