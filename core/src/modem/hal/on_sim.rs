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
        // The first pass runs before the SIM is ready (CLIP and SMS fail);
        // the re-run at `SMS DONE` is clean.
        s.until(30_000, |o| *o == Output::Event(Event::Configured { issues: vec![] }));
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

/// As on hardware: the first configuration pass runs before the SIM is
/// ready, so caller ID fails; the pass re-runs at `+CPIN: READY`. The SMS
/// settings fail until `SMS DONE` (a guess, see `sim.rs`), and are applied
/// by the re-run then.
#[test]
fn sim_dependent_settings_are_configured_once_ready() {
    let mut s = Stack { hal: Hal::new(), t: SimTransport::new(Sim::new(SimConfig::default())), out: vec![] };
    s.power_on();
    let sms_busy: Vec<String> =
        ["AT+CMGF=0", "AT+CMGF?", "AT+CNMI=2,1,0,0,0"].iter().map(|c| format!("{c}: Cms(\"SIM busy\")")).collect();
    let first = s.until(30_000, |o| matches!(o, Output::Event(Event::Configured { .. })));
    let mut issues = vec!["AT+CLIP=1: Error".to_string(), "AT+CLIP?: Error".into()];
    issues.extend(sms_busy.clone());
    assert_eq!(first, Output::Event(Event::Configured { issues }));
    s.until(10_000, |o| *o == Output::Event(Event::Urc(Unsolicited::Sim("READY".into()))));
    s.until(5_000, |o| *o == Output::Event(Event::Configured { issues: sms_busy.clone() }));
    let now = s.t.now();
    s.t.sim.incoming_call("07700900123", now);
    s.until(1000, |o| matches!(o, Output::Event(Event::Urc(Unsolicited::CallerId { .. }))));
    s.until(5_000, |o| *o == Output::Event(Event::Configured { issues: vec![] }));
}

fn submit_pdu() -> crate::modem::sms_pdu::SubmitPdu {
    crate::modem::sms_pdu::encode_submit("07700900123", "hello", 1).unwrap().remove(0)
}

/// Until a pass has read back PDU mode, a send is refused without being
/// written: in text mode the modem would text the PDU's hex to the
/// "number" in `AT+CMGS=<length>`.
#[test]
fn sms_is_only_sent_once_pdu_mode_is_confirmed() {
    let mut s = Stack { hal: Hal::new(), t: SimTransport::new(Sim::new(SimConfig::default())), out: vec![] };
    s.power_on();
    s.until(30_000, |o| matches!(o, Output::Event(Event::Configured { .. })));
    let early = s.submit(Request::SendMessage(submit_pdu()));
    assert_eq!(s.done(early, 1000), Err(RequestError::NotSent));
    s.until(15_000, |o| *o == Output::Event(Event::Configured { issues: vec![] }));
    let id = s.submit(Request::SendMessage(submit_pdu()));
    assert_eq!(s.done(id, 5000), Ok(Response::MessageRef(0)));
    let sent = s.t.sim.sent_sms();
    assert_eq!(sent, [crate::modem::sim::SentSms::Pdu(submit_pdu().hex().into())]);
}

/// A restart loses PDU mode. A send queued across it is refused (not
/// written in text mode) until a pass reads PDU mode back, after `SMS DONE`.
#[test]
fn pdu_mode_is_restored_after_a_restart() {
    let mut s = Stack::configured();
    let now = s.t.now();
    s.t.sim.cut_power(now);
    s.t.sim.power_on(now);
    s.until(30_000, |o| *o == Output::Event(Event::Restarted));
    let queued = s.submit(Request::SendMessage(submit_pdu()));
    assert_eq!(s.done(queued, 30_000), Err(RequestError::NotSent));
    assert_eq!(s.t.sim.sent_sms(), []);
    s.until(30_000, |o| *o == Output::Event(Event::Configured { issues: vec![] }));
    let id = s.submit(Request::SendMessage(submit_pdu()));
    assert_eq!(s.done(id, 5000), Ok(Response::MessageRef(0)));
    assert!(matches!(s.t.sim.sent_sms(), [crate::modem::sim::SentSms::Pdu(_)]));
}

/// Receive, list, read, delete through the whole stack.
#[test]
fn sms_round_trip_through_the_stack() {
    let mut s = Stack::configured();
    let pdu = "0791449785788997040C914477000910320000620102516080400CC4BC3B4C0F8F4169372806";
    let now = s.t.now();
    assert_eq!(s.t.sim.receive_sms(pdu, now), Some(1));
    s.until(1000, |o| {
        *o == Output::Event(Event::Urc(Unsolicited::NewSms { storage: "SM".into(), index: 1 }))
    });
    let list = s.submit(Request::ListMessages);
    let Ok(Response::Messages(m)) = s.done(list, 1000) else { panic!() };
    assert_eq!((m.len(), m[0].index, m[0].pdu.as_str()), (1, 1, pdu));
    let read = s.submit(Request::ReadMessage(1));
    assert!(matches!(s.done(read, 1000), Ok(Response::Messages(m)) if m.len() == 1));
    let del = s.submit(Request::DeleteMessage(1));
    assert_eq!(s.done(del, 1000), Ok(Response::Done));
    let empty = s.submit(Request::ReadMessage(1));
    assert!(matches!(s.done(empty, 1000), Err(RequestError::Failed(_))));
    assert!(s.t.sim.messages().is_empty());
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
