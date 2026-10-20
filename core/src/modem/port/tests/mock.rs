//! A mock modem at the request/event level, for testing services and
//! programs (L3/L4) with exact control over what the modem says. Test
//! builds only.
//!
//! [`MockHal`] implements [`ModemPort`]. It starts configured. By default
//! every request succeeds with a plausible response (from a small state:
//! the call list and the speaker gain, both set by the test). Tests can:
//!   - script the next matching request's result ([`MockHal::on`]),
//!     including any failure (`Unknown`, `Failed`, `NotSent`, `Malformed`);
//!   - hold replies ([`MockHal::hold`]) so events can be emitted before a
//!     response, as the real modem does (`NO CARRIER` before `CHUP`'s `OK`),
//!     then release them one by one;
//!   - emit events and call-state changes;
//!   - simulate a restart or link loss, which settles held replies as the
//!     real HAL would.
//!
//! It keeps the [`ModemPort`] contract: exactly one `Done` per request, in
//! submission order. It does not model the modem: a `Dial` does not create
//! a call by itself (see `core/src/modem/sim.rs` for that). `SimModem` and
//! `MockHal` are checked against each other in `contract.rs` next to this.

use std::collections::VecDeque;

use crate::modem::hal::{Event, RequestError, RequestId};
use crate::modem::link::{UnknownReason, UnsyncReason};
use crate::modem::port::{ModemPort, Notice};
use crate::modem::request::{Gain, Request, Response};
use crate::modem::unsolicited::{CallInfo, CallMode, CallState, Direction, Unsolicited};

type Matcher = Box<dyn Fn(&Request) -> bool>;

pub(crate) struct MockHal {
    next_id: u64,
    out: VecDeque<Notice>,
    sent: Vec<Request>,
    scripted: Vec<(Matcher, Result<Response, RequestError>)>,
    holding: bool,
    held: VecDeque<(RequestId, Result<Response, RequestError>)>,
    calls: Vec<CallInfo>,
    gain: Gain,
}

impl MockHal {
    /// Configured, no calls, gain at maximum.
    pub fn new() -> Self {
        MockHal {
            next_id: 0,
            out: VecDeque::new(),
            sent: Vec::new(),
            scripted: Vec::new(),
            holding: false,
            held: VecDeque::new(),
            calls: Vec::new(),
            gain: Gain::MAX,
        }
    }

    /// Every request submitted so far, in order.
    pub fn sent(&self) -> &[Request] {
        &self.sent
    }

    /// The next request matching `pred` gets `result` (once). Earlier
    /// scripts take precedence.
    pub fn on(&mut self, pred: impl Fn(&Request) -> bool + 'static, result: Result<Response, RequestError>) {
        self.scripted.push((Box::new(pred), result));
    }

    /// Hold replies until released (events still go out immediately).
    pub fn hold(&mut self) {
        self.holding = true;
    }

    /// Release the oldest held reply. Returns whether there was one.
    pub fn release_one(&mut self) -> bool {
        match self.held.pop_front() {
            Some((id, result)) => {
                self.out.push_back(Notice::Done { id, result });
                true
            }
            None => false,
        }
    }

    /// Stop holding and release everything held.
    pub fn release_all(&mut self) {
        self.holding = false;
        while self.release_one() {}
    }

    pub fn emit(&mut self, event: Event) {
        self.out.push_back(Notice::Event(event));
    }

    pub fn emit_urc(&mut self, urc: Unsolicited) {
        self.emit(Event::Urc(urc));
    }

    /// A call changed state: update the call list (a `Disconnected` call
    /// leaves it) and emit the `+CLCC` URC, as the modem does.
    pub fn call_update(&mut self, call: CallInfo) {
        self.calls.retain(|c| c.index != call.index);
        if call.state != CallState::Disconnected {
            self.calls.push(call.clone());
        }
        self.emit_urc(Unsolicited::Call(call));
    }

    /// Set the call list `ListCalls` reports, without emitting anything
    /// (e.g. a change whose URC was lost).
    pub fn set_calls(&mut self, calls: Vec<CallInfo>) {
        self.calls = calls;
    }

    pub fn set_gain(&mut self, gain: Gain) {
        self.gain = gain;
    }

    /// The modem rebooted: held replies settle as `Unknown(Reset)`, then
    /// `Restarted` and `Configured`, as the real HAL reports. Calls die
    /// with the modem.
    pub fn restart(&mut self) {
        self.settle_held(UnknownReason::Reset);
        self.calls.clear();
        self.emit(Event::Restarted);
        self.emit(Event::Configured { issues: vec![] });
    }

    /// The link lost sync (timeout / lost bytes): held replies settle as
    /// `Unknown`, then `LinkLost` and, once resynced, `Configured`.
    pub fn link_lost(&mut self, reason: UnsyncReason) {
        let why = match reason {
            UnsyncReason::Timeout => UnknownReason::Timeout,
            UnsyncReason::BytesLost => UnknownReason::BytesLost,
            UnsyncReason::StrayFinal | UnsyncReason::Reset => UnknownReason::Reset,
        };
        self.settle_held(why);
        if reason == UnsyncReason::BytesLost {
            self.emit(Event::BytesLost);
        }
        self.emit(Event::LinkLost(reason));
        self.emit(Event::Configured { issues: vec![] });
    }

    /// The oldest held reply is the request in flight: it settles as
    /// `Unknown`. Those behind it were never sent; the real HAL sends them
    /// after reconfiguring, so they keep their results.
    fn settle_held(&mut self, why: UnknownReason) {
        if let Some((id, _)) = self.held.pop_front() {
            self.out.push_back(Notice::Done { id, result: Err(RequestError::Unknown(why)) });
        }
    }

    // --- real sequences (checked against the simulator in `contract.rs`) ---

    /// Someone calls: `RING`, caller ID, then the incoming `+CLCC`.
    pub fn incoming_call(&mut self, number: &str) {
        self.emit_urc(Unsolicited::Ring);
        self.emit_urc(Unsolicited::CallerId { number: Some(number.to_string()) });
        self.call_update(CallInfo {
            index: 1,
            direction: Direction::Incoming,
            state: CallState::Incoming,
            mode: CallMode::Voice,
            multiparty: false,
            number: Some(number.to_string()),
        });
    }

    /// The far end answers our outgoing call (call 1 must exist).
    pub fn remote_answer(&mut self) {
        let mut c = self.calls.first().cloned().expect("a call to answer");
        c.state = CallState::Active;
        self.emit_urc(Unsolicited::VoiceCallBegin);
        self.call_update(c.clone());
        self.emit_urc(Unsolicited::ConnectedLine { number: c.number });
    }

    /// The call ends (far end hangs up, or the URCs of our own `HangUp`,
    /// which the real modem sends *before* its response — use `hold`).
    /// `duration_s` is `None` for a call that was never answered.
    pub fn call_ended(&mut self, duration_s: Option<u32>) {
        let mut c = self.calls.first().cloned().expect("a call to end");
        c.state = CallState::Disconnected;
        self.call_update(c);
        self.emit_urc(Unsolicited::VoiceCallEnd { duration_s });
        self.emit_urc(Unsolicited::NoCarrier);
    }

    fn default_response(&self, r: &Request) -> Response {
        use Request as R;
        match r {
            R::ListCalls => Response::Calls(self.calls.clone()),
            R::QueryOutputGain => Response::Gain(self.gain),
            R::QueryVerboseErrors => Response::Setting(2),
            R::QueryVoiceHangUp => Response::Setting(0),
            R::QueryCallerId => Response::Setting(1),
            R::Identify => Response::Lines(vec!["Model: A7682E".into()]),
            R::Firmware => Response::Firmware("A011B15A7682M7".into()),
            R::Dial(_)
            | R::Answer
            | R::HangUp
            | R::SetOutputGain(_)
            | R::EchoOff
            | R::SetVerboseErrors
            | R::SetVoiceHangUp
            | R::SetCallerId => Response::Done,
        }
    }
}

impl Default for MockHal {
    fn default() -> Self {
        Self::new()
    }
}

impl ModemPort for MockHal {
    fn submit(&mut self, request: Request) -> RequestId {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        let result = match self.scripted.iter().position(|(m, _)| m(&request)) {
            Some(i) => self.scripted.remove(i).1,
            None => {
                if let Request::SetOutputGain(g) = &request {
                    self.gain = *g;
                }
                Ok(self.default_response(&request))
            }
        };
        self.sent.push(request);
        if self.holding {
            self.held.push_back((id, result));
        } else {
            self.out.push_back(Notice::Done { id, result });
        }
        id
    }

    fn poll(&mut self) -> Option<Notice> {
        self.out.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::link::Failure;
    use crate::modem::request::PhoneNumber;

    fn drain(f: &mut MockHal) -> Vec<Notice> {
        std::iter::from_fn(|| f.poll()).collect()
    }

    fn call(state: CallState) -> CallInfo {
        CallInfo {
            index: 1,
            direction: Direction::Outgoing,
            state,
            mode: CallMode::Voice,
            multiparty: false,
            number: Some("07700900123".into()),
        }
    }

    #[test]
    fn defaults_succeed_with_plausible_responses() {
        let mut f = MockHal::new();
        let a = f.submit(Request::HangUp);
        let b = f.submit(Request::ListCalls);
        let c = f.submit(Request::QueryOutputGain);
        assert_eq!(
            drain(&mut f),
            [
                Notice::Done { id: a, result: Ok(Response::Done) },
                Notice::Done { id: b, result: Ok(Response::Calls(vec![])) },
                Notice::Done { id: c, result: Ok(Response::Gain(Gain::MAX)) },
            ]
        );
        assert_eq!(f.sent(), [Request::HangUp, Request::ListCalls, Request::QueryOutputGain]);
    }

    #[test]
    fn scripted_results_apply_once_to_the_first_match() {
        let mut f = MockHal::new();
        f.on(|r| matches!(r, Request::HangUp), Err(RequestError::Unknown(UnknownReason::Timeout)));
        let a = f.submit(Request::ListCalls);
        let b = f.submit(Request::HangUp);
        let c = f.submit(Request::HangUp);
        let results: Vec<_> = drain(&mut f)
            .into_iter()
            .map(|n| match n {
                Notice::Done { id, result } => (id, result),
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(
            results,
            [
                (a, Ok(Response::Calls(vec![]))),
                (b, Err(RequestError::Unknown(UnknownReason::Timeout))),
                (c, Ok(Response::Done))
            ]
        );
    }

    #[test]
    fn held_replies_let_events_arrive_first() {
        let mut f = MockHal::new();
        f.call_update(call(CallState::Active));
        drain(&mut f);
        f.hold();
        let id = f.submit(Request::HangUp);
        f.call_update(call(CallState::Disconnected));
        f.emit_urc(Unsolicited::NoCarrier);
        assert!(f.release_one());
        let n = drain(&mut f);
        assert_eq!(n.last(), Some(&Notice::Done { id, result: Ok(Response::Done) }));
        assert_eq!(n[1], Notice::Event(Event::Urc(Unsolicited::NoCarrier)));
        let list = f.submit(Request::ListCalls);
        f.release_all();
        assert_eq!(drain(&mut f), [Notice::Done { id: list, result: Ok(Response::Calls(vec![])) }]);
    }

    #[test]
    fn call_list_follows_call_updates() {
        let mut f = MockHal::new();
        f.call_update(call(CallState::Dialling));
        f.call_update(call(CallState::Active));
        let id = f.submit(Request::ListCalls);
        let n = drain(&mut f);
        assert_eq!(n.last(), Some(&Notice::Done { id, result: Ok(Response::Calls(vec![call(CallState::Active)])) }));
    }

    #[test]
    fn restart_settles_the_in_flight_request() {
        let mut f = MockHal::new();
        f.call_update(call(CallState::Active));
        drain(&mut f);
        f.hold();
        let a = f.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
        let b = f.submit(Request::ListCalls);
        f.restart();
        f.release_all();
        assert_eq!(
            drain(&mut f),
            [
                Notice::Done { id: a, result: Err(RequestError::Unknown(UnknownReason::Reset)) },
                Notice::Event(Event::Restarted),
                Notice::Event(Event::Configured { issues: vec![] }),
                Notice::Done { id: b, result: Ok(Response::Calls(vec![call(CallState::Active)])) },
            ]
        );
    }

    #[test]
    fn failures_can_be_scripted() {
        let mut f = MockHal::new();
        f.on(|r| matches!(r, Request::Dial(_)), Err(RequestError::Failed(Failure::Code("BUSY".into()))));
        let id = f.submit(Request::Dial(PhoneNumber::parse("07700900123").unwrap()));
        assert_eq!(
            drain(&mut f),
            [Notice::Done { id, result: Err(RequestError::Failed(Failure::Code("BUSY".into()))) }]
        );
    }

    #[test]
    fn set_gain_is_remembered() {
        let mut f = MockHal::new();
        f.submit(Request::SetOutputGain(Gain::new(3).unwrap()));
        let id = f.submit(Request::QueryOutputGain);
        let n = drain(&mut f);
        assert_eq!(n[1], Notice::Done { id, result: Ok(Response::Gain(Gain::new(3).unwrap())) });
    }

    /// A call change whose URC was lost: only a query reveals it.
    #[test]
    fn set_calls_changes_the_list_silently() {
        let mut f = MockHal::new();
        f.set_calls(vec![call(CallState::Active)]);
        assert_eq!(drain(&mut f), []);
        let id = f.submit(Request::ListCalls);
        assert_eq!(drain(&mut f), [Notice::Done { id, result: Ok(Response::Calls(vec![call(CallState::Active)])) }]);
    }

    #[test]
    fn set_gain_is_reported_by_queries() {
        let mut f = MockHal::new();
        f.set_gain(Gain::new(2).unwrap());
        let id = f.submit(Request::QueryOutputGain);
        assert_eq!(drain(&mut f), [Notice::Done { id, result: Ok(Response::Gain(Gain::new(2).unwrap())) }]);
    }

    #[test]
    fn link_lost_settles_the_in_flight_request_and_resyncs() {
        let mut f = MockHal::new();
        f.hold();
        let a = f.submit(Request::HangUp);
        let b = f.submit(Request::ListCalls);
        f.link_lost(UnsyncReason::BytesLost);
        f.release_all();
        assert_eq!(
            drain(&mut f),
            [
                Notice::Done { id: a, result: Err(RequestError::Unknown(UnknownReason::BytesLost)) },
                Notice::Event(Event::BytesLost),
                Notice::Event(Event::LinkLost(UnsyncReason::BytesLost)),
                Notice::Event(Event::Configured { issues: vec![] }),
                Notice::Done { id: b, result: Ok(Response::Calls(vec![])) },
            ]
        );
    }
}
