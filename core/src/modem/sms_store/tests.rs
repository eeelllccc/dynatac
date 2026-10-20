//! `SmsStore` on `MockHal` (exact control), on the simulated modem (the
//! whole stack), and under faults (the real HAL over a faulty link).

use super::*;
use crate::modem::link::UnknownReason;
use crate::modem::port::tests::{MockHal, SimModem};
use crate::modem::sim::{SentSms, SimConfig, SimSms};
use crate::modem::sms_storage::{Status, Stored};

const NUMBER: &str = "07700900123";

fn bcd2(v: u32) -> String {
    format!("{}{}", v % 10, v / 10)
}

/// A distinct SMS-DELIVER ("Dynatac in 1" from +447700900123), told apart
/// by its timestamp: `n` seconds after 2026-10-20 00:00:00.
fn pdu_with_pid(n: u32, pid: u8) -> String {
    let scts: String = [26, 10, 20, n / 3600 % 24, n / 60 % 60, n % 60].map(bcd2).concat();
    format!("0791449785788997040C91447700091032{pid:02X}00{scts}400CC4BC3B4C0F8F4169372806")
}

fn pdu(n: u32) -> String {
    pdu_with_pid(n, 0)
}

fn type_0(n: u32) -> String {
    pdu_with_pid(n, PID_TYPE_0)
}

#[test]
fn test_pdus_decode() {
    for p in [pdu(0), pdu(86_399), type_0(5)] {
        let Ok(Pdu::Deliver(d)) = sms_pdu::decode(&p) else { panic!("{p}") };
        assert_eq!(d.from, "+447700900123");
    }
    assert_ne!(pdu(1), pdu(2));
    assert!(is_type_0(&type_0(1)) && !is_type_0(&pdu(1)));
}

fn deletes(f: &MockHal) -> Vec<u32> {
    f.sent()
        .iter()
        .filter_map(|r| match r {
            Request::DeleteMessage(i) => Some(*i),
            _ => None,
        })
        .collect()
}

fn listings(f: &MockHal) -> usize {
    f.sent().iter().filter(|r| **r == Request::ListMessages).count()
}

fn sends(f: &MockHal) -> Vec<SubmitPdu> {
    f.sent()
        .iter()
        .filter_map(|r| match r {
            Request::SendMessage(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

struct Rig {
    f: MockHal,
    s: SmsStore,
    now: Millis,
    events: Vec<SmsEvent>,
}

impl Rig {
    /// Not yet configured.
    fn cold() -> Self {
        Rig { f: MockHal::new(), s: SmsStore::new(7), now: 0, events: vec![] }
    }

    /// Configured, first round done (empty SIM).
    fn new() -> Self {
        let mut r = Rig::cold();
        r.configured();
        assert_eq!(listings(&r.f), 1);
        r
    }

    fn configured(&mut self) {
        self.f.emit(Event::Configured { issues: vec![] });
        self.pump();
    }

    fn pump(&mut self) {
        while let Some(n) = self.f.poll() {
            self.s.handle(&mut self.f, n, self.now);
        }
        while let Some(e) = self.s.poll_event() {
            self.events.push(e);
        }
    }

    fn run(&mut self, ms: Millis) {
        let end = self.now + ms;
        while self.now < end {
            self.now += 100;
            self.s.tick(&mut self.f, self.now);
            self.pump();
        }
    }

    fn save(&mut self, pdu: &str) {
        self.s.saved(&mut self.f, pdu, self.now);
        self.pump();
    }

    fn received(&mut self) -> Vec<String> {
        let (r, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.events).into_iter().partition(|e| matches!(e, SmsEvent::Received { .. }));
        self.events = rest;
        r.into_iter()
            .map(|e| match e {
                SmsEvent::Received { pdu } => pdu,
                _ => unreachable!(),
            })
            .collect()
    }

    fn sent_results(&mut self) -> Vec<(SendId, SendResult)> {
        let (r, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.events).into_iter().partition(|e| matches!(e, SmsEvent::Sent { .. }));
        self.events = rest;
        r.into_iter()
            .map(|e| match e {
                SmsEvent::Sent { id, result } => (id, result),
                _ => unreachable!(),
            })
            .collect()
    }

    fn sim_pdus(&self) -> Vec<&str> {
        self.f.messages().iter().map(|m| m.pdu.as_str()).collect()
    }
}

fn stored(index: u32, pdu: String) -> Stored {
    Stored { index, status: Status::Unread, pdu }
}

// --- receiving ---

/// Messages that arrived while the device was off are offered at the
/// first `Configured`, and stay on the SIM until saved.
#[test]
fn stored_messages_are_offered_then_deleted_once_saved() {
    let mut r = Rig::cold();
    r.f.set_messages(vec![stored(1, pdu(1)), stored(2, pdu(2))]);
    r.configured();
    assert_eq!(r.received(), [pdu(1), pdu(2)]);
    r.run(5000);
    assert_eq!(deletes(&r.f), Vec::<u32>::new(), "nothing deleted before it is saved");
    r.save(&pdu(2));
    assert_eq!(deletes(&r.f), [2]);
    assert_eq!(r.sim_pdus(), [pdu(1)]);
    r.save(&pdu(1));
    assert_eq!(deletes(&r.f), [2, 1]);
    assert!(r.f.messages().is_empty());
    assert_eq!(r.received(), Vec::<String>::new(), "offered once");
}

#[test]
fn new_message_indication_starts_a_round() {
    let mut r = Rig::new();
    r.f.receive_sms(&pdu(1));
    r.pump();
    assert_eq!(r.received(), [pdu(1)]);
    r.save(&pdu(1));
    assert!(r.f.messages().is_empty());
}

/// An unsaved message is not offered again on later rounds.
#[test]
fn unsaved_messages_are_offered_once() {
    let mut r = Rig::new();
    r.f.receive_sms(&pdu(1));
    r.pump();
    r.f.receive_sms(&pdu(2));
    r.pump();
    r.run(3 * POLL_MS);
    assert!(listings(&r.f) >= 5);
    assert_eq!(r.received(), [pdu(1), pdu(2)]);
    assert_eq!(r.f.messages().len(), 2);
}

/// "Silent" messages are deleted without being offered.
#[test]
fn type_0_messages_are_deleted_unseen() {
    let mut r = Rig::new();
    r.f.receive_sms(&type_0(1));
    r.f.receive_sms(&pdu(2));
    r.pump();
    assert_eq!(r.received(), [pdu(2)]);
    assert_eq!(deletes(&r.f), [1]);
    assert_eq!(r.sim_pdus(), [pdu(2)]);
}

/// A `+CMTI` while the listing is in flight may not be in it: list again.
#[test]
fn hint_during_a_round_runs_another() {
    let mut r = Rig::new();
    r.f.hold();
    r.f.receive_sms(&pdu(1));
    r.pump(); // listing (of pdu(1) only) submitted, held
    r.f.receive_sms(&pdu(2));
    r.pump(); // its `+CMTI` arrives mid-round
    assert_eq!(listings(&r.f), 2);
    r.f.release_all();
    r.pump();
    assert_eq!(listings(&r.f), 3);
    assert_eq!(r.received(), [pdu(1), pdu(2)]);
}

/// Saving during the delete phase runs another round for it.
#[test]
fn saved_mid_round_is_deleted_by_the_next() {
    let mut r = Rig::new();
    r.f.receive_sms(&pdu(1));
    r.f.receive_sms(&pdu(2));
    r.pump();
    r.f.hold();
    r.save(&pdu(1)); // round: listing held
    r.f.release_one();
    r.pump(); // listing handled: delete of slot 1 submitted, held
    r.save(&pdu(2)); // mid-round
    assert_eq!(deletes(&r.f), [1]);
    r.f.release_all();
    r.pump();
    assert_eq!(deletes(&r.f), [1, 2]);
    assert!(r.f.messages().is_empty());
}

#[test]
fn failed_listing_is_retried() {
    let mut r = Rig::cold();
    r.f.set_messages(vec![stored(1, pdu(1))]);
    let busy = Err(RequestError::Failed(Failure::Cms("SIM busy".into())));
    r.f.on(|q| *q == Request::ListMessages, busy);
    r.configured();
    assert_eq!(r.received(), Vec::<String>::new());
    r.run(RETRY_MS - 200);
    assert_eq!(listings(&r.f), 1);
    r.run(300);
    assert_eq!(listings(&r.f), 2);
    assert_eq!(r.received(), [pdu(1)]);
}

/// After a failed round, `SMS DONE` retries at once (the usual boot case:
/// the first listing comes before the SIM's SMS storage is ready).
#[test]
fn sms_ready_retries_at_once() {
    let mut r = Rig::cold();
    r.f.set_messages(vec![stored(1, pdu(1))]);
    let busy = Err(RequestError::Failed(Failure::Cms("SIM busy".into())));
    r.f.on(|q| *q == Request::ListMessages, busy);
    r.configured();
    r.f.emit_urc(Unsolicited::SmsReady);
    r.pump();
    assert_eq!(r.received(), [pdu(1)]);
}

/// A delete whose outcome is unknown may have freed the slot, and a new
/// message may take it at once. Retrying the delete blindly would destroy
/// that message; the next round lists first.
#[test]
fn unknown_delete_is_not_retried_blindly() {
    let mut r = Rig::new();
    r.f.receive_sms(&pdu(1));
    r.pump();
    r.received();
    // The delete is carried out (slot 1 freed) but its reply is lost …
    r.f.on(|q| *q == Request::DeleteMessage(1), Err(RequestError::Unknown(UnknownReason::Timeout)));
    r.save(&pdu(1));
    r.f.set_messages(vec![]);
    // … and a new message lands in slot 1 before the next round.
    r.f.set_messages(vec![stored(1, pdu(2))]);
    r.run(RETRY_MS + 1000);
    assert_eq!(deletes(&r.f), [1], "no second delete of slot 1");
    assert_eq!(r.received(), [pdu(2)]);
    assert_eq!(r.sim_pdus(), [pdu(2)]);
}

/// A failed delete is retried by a later round, after a fresh listing.
#[test]
fn failed_delete_is_retried_after_listing_again() {
    let mut r = Rig::new();
    r.f.receive_sms(&pdu(1));
    r.pump();
    r.f.on(|q| *q == Request::DeleteMessage(1), Err(RequestError::Failed(Failure::Error)));
    let before = listings(&r.f);
    r.save(&pdu(1));
    assert_eq!(r.sim_pdus(), [pdu(1)]);
    r.run(RETRY_MS + 200);
    assert_eq!(listings(&r.f), before + 2);
    assert_eq!(deletes(&r.f), [1, 1]);
    assert!(r.f.messages().is_empty());
}

/// A lost `+CMTI` is caught by the poll.
#[test]
fn polls_in_case_an_indication_was_lost() {
    let mut r = Rig::new();
    r.f.set_messages(vec![stored(3, pdu(3))]);
    r.run(POLL_MS - 200);
    assert_eq!(r.received(), Vec::<String>::new());
    r.run(300);
    assert_eq!(r.received(), [pdu(3)]);
}

#[test]
fn lost_bytes_start_a_round() {
    let mut r = Rig::new();
    r.f.set_messages(vec![stored(3, pdu(3))]);
    r.f.emit(Event::BytesLost);
    r.pump();
    assert_eq!(r.received(), [pdu(3)]);
}

/// Nothing is sent before the first `Configured`; hints meanwhile are
/// served by the round it starts.
#[test]
fn nothing_happens_before_configured() {
    let mut r = Rig::cold();
    r.f.receive_sms(&pdu(1));
    r.pump();
    r.run(2 * POLL_MS);
    assert_eq!(r.f.sent(), []);
    r.configured();
    assert_eq!(r.received(), [pdu(1)]);
}

#[test]
fn saving_something_never_offered_does_nothing() {
    let mut r = Rig::new();
    r.f.set_messages(vec![stored(1, pdu(1))]);
    r.save(&pdu(1));
    assert_eq!(listings(&r.f), 1);
    assert_eq!(deletes(&r.f), Vec::<u32>::new());
}

#[test]
fn sim_usage_is_reported() {
    let mut r = Rig::new();
    assert_eq!(r.s.sim_used(), Some(0));
    r.f.receive_sms(&pdu(1));
    r.pump();
    assert_eq!(r.s.sim_used(), Some(1));
}

// --- sending ---

#[test]
fn single_part_text_is_sent() {
    let mut r = Rig::new();
    let id = r.s.send(&mut r.f, NUMBER, "hello").unwrap();
    r.pump();
    assert_eq!(sends(&r.f), sms_pdu::encode_submit(NUMBER, "hello", 7).unwrap());
    assert_eq!(r.sent_results(), [(id, SendResult::Sent)]);
    assert_eq!(r.s.outbox().count(), 0);
}

/// One part at a time, in order; texts queue behind each other.
#[test]
fn parts_go_out_one_at_a_time() {
    let mut r = Rig::new();
    r.f.hold();
    let long = "x".repeat(200);
    let a = r.s.send(&mut r.f, NUMBER, &long).unwrap();
    let b = r.s.send(&mut r.f, NUMBER, "after").unwrap();
    r.pump();
    assert_eq!(sends(&r.f).len(), 1);
    assert_eq!(r.s.outbox().collect::<Vec<_>>(), [(a, 0, 2), (b, 0, 1)]);
    r.f.release_one();
    r.pump();
    assert_eq!(sends(&r.f).len(), 2);
    r.f.release_all();
    r.pump();
    let mut want = sms_pdu::encode_submit(NUMBER, &long, 7).unwrap();
    want.extend(sms_pdu::encode_submit(NUMBER, "after", 8).unwrap());
    assert_eq!(sends(&r.f), want);
    assert_eq!(r.sent_results(), [(a, SendResult::Sent), (b, SendResult::Sent)]);
}

/// Each multipart text gets its own reference (single-part texts use none).
#[test]
fn multipart_references_count_up() {
    let mut r = Rig::new();
    for t in ["a".repeat(200), "short".into(), "b".repeat(200)] {
        r.s.send(&mut r.f, NUMBER, &t).unwrap();
        r.pump();
    }
    let refs: Vec<_> = sends(&r.f)
        .iter()
        .filter_map(|p| match sms_pdu::decode(p.hex()) {
            Ok(Pdu::Submit(s)) => s.concat.map(|c| c.reference),
            _ => None,
        })
        .collect();
    assert_eq!(refs, [7, 7, 8, 8]);
}

#[test]
fn refused_part_fails_the_text_and_drops_the_rest() {
    let mut r = Rig::new();
    let refused = Failure::Cms("Unknown error".into());
    r.f.on(|q| matches!(q, Request::SendMessage(_)), Ok(Response::MessageRef(1)));
    r.f.on(|q| matches!(q, Request::SendMessage(_)), Err(RequestError::Failed(refused.clone())));
    let a = r.s.send(&mut r.f, NUMBER, &"x".repeat(400)).unwrap(); // 3 parts
    let b = r.s.send(&mut r.f, NUMBER, "next").unwrap();
    r.pump();
    assert_eq!(sends(&r.f).len(), 3, "parts 1 and 2 of a, then b");
    assert_eq!(
        r.sent_results(),
        [(a, SendResult::Failed { parts_sent: 1, error: SendError::Refused(refused) }), (b, SendResult::Sent)]
    );
}

/// The part may have gone out: report, never resend.
#[test]
fn unknown_outcome_is_reported_not_retried() {
    let mut r = Rig::new();
    r.f.on(|q| matches!(q, Request::SendMessage(_)), Err(RequestError::Unknown(UnknownReason::Timeout)));
    let a = r.s.send(&mut r.f, NUMBER, "hello").unwrap();
    r.pump();
    r.configured();
    r.run(5000);
    assert_eq!(sends(&r.f).len(), 1);
    assert_eq!(r.sent_results(), [(a, SendResult::Unknown { parts_sent: 0 })]);
}

/// `NotSent` (e.g. PDU mode not yet confirmed after a restart): waits for
/// the next `Configured`, then tries again.
#[test]
fn not_sent_waits_for_the_next_configured() {
    let mut r = Rig::new();
    r.f.on(|q| matches!(q, Request::SendMessage(_)), Err(RequestError::NotSent));
    let a = r.s.send(&mut r.f, NUMBER, "hello").unwrap();
    r.pump();
    r.run(5000);
    assert_eq!(sends(&r.f).len(), 1);
    assert_eq!(r.s.outbox().count(), 1);
    r.configured();
    assert_eq!(sends(&r.f).len(), 2);
    assert_eq!(r.sent_results(), [(a, SendResult::Sent)]);
}

#[test]
fn not_sent_gives_up_after_max_attempts() {
    let mut r = Rig::new();
    for _ in 0..MAX_ATTEMPTS {
        r.f.on(|q| matches!(q, Request::SendMessage(_)), Err(RequestError::NotSent));
    }
    let a = r.s.send(&mut r.f, NUMBER, "hello").unwrap();
    r.pump();
    for _ in 0..MAX_ATTEMPTS {
        r.configured();
    }
    assert_eq!(sends(&r.f).len(), MAX_ATTEMPTS as usize);
    assert_eq!(r.sent_results(), [(a, SendResult::Failed { parts_sent: 0, error: SendError::NotSent })]);
}

#[test]
fn texts_queue_while_the_link_is_down() {
    let mut r = Rig::cold();
    let a = r.s.send(&mut r.f, NUMBER, "hello").unwrap();
    r.pump();
    assert_eq!(sends(&r.f), Vec::<SubmitPdu>::new());
    r.configured();
    assert_eq!(r.sent_results(), [(a, SendResult::Sent)]);
}

#[test]
fn unencodable_texts_are_refused() {
    let mut r = Rig::new();
    assert_eq!(r.s.send(&mut r.f, "07700 900123", "x"), Err(EncodeError::BadNumber));
    assert_eq!(r.s.send(&mut r.f, NUMBER, ""), Err(EncodeError::Empty));
    assert_eq!(sends(&r.f), Vec::<SubmitPdu>::new());
}

// --- on the simulated modem ---

struct SimRig {
    m: SimModem,
    s: SmsStore,
    events: Vec<SmsEvent>,
}

impl SimRig {
    /// Powered on with `on_sim` already stored; run through boot.
    fn new(on_sim: Vec<SimSms>) -> Self {
        let mut m = SimModem::powered_on(SimConfig::default());
        for sms in on_sim {
            m.t.sim.store_sms(sms);
        }
        let mut r = SimRig { m, s: SmsStore::new(1), events: vec![] };
        r.run(20_000);
        r
    }

    fn run(&mut self, ms: Millis) {
        let end = self.m.now() + ms;
        while self.m.now() < end {
            self.m.run(50);
            let now = self.m.now();
            while let Some(n) = self.m.poll() {
                self.s.handle(&mut self.m, n, now);
            }
            self.s.tick(&mut self.m, now);
            while let Some(e) = self.s.poll_event() {
                self.events.push(e);
            }
        }
    }

    /// Save everything offered so far.
    fn save_all(&mut self) -> Vec<String> {
        let now = self.m.now();
        let mut saved = vec![];
        for e in std::mem::take(&mut self.events) {
            if let SmsEvent::Received { pdu } = e {
                self.s.saved(&mut self.m, &pdu, now);
                saved.push(pdu);
            } else {
                self.events.push(e);
            }
        }
        saved
    }
}

/// The real boot: the first listing comes before `SMS DONE` and fails;
/// the store retries at `SMS DONE` and empties the SIM.
#[test]
fn sim_messages_stored_while_off_are_moved_to_the_device() {
    let on_sim = (1..=3).map(|i| SimSms { index: i, status: Status::Read, pdu: pdu(i) }).collect();
    let mut r = SimRig::new(on_sim);
    assert_eq!(r.save_all(), [pdu(1), pdu(2), pdu(3)]);
    r.run(2000);
    assert!(r.m.t.sim.messages().is_empty());
}

/// The three parts of a long text, arriving out of order (as in capture
/// 14), plus a silent message in between.
#[test]
fn sim_parts_arriving_out_of_order() {
    let mut r = SimRig::new(vec![]);
    let parts: Vec<String> = [(1, 3), (3, 3), (2, 3)]
        .iter()
        .map(|&(seq, total)| {
            // UDH 05 00 03 2A <total> <seq>, then "x" (one fill bit).
            format!("0791449785788997440C9144770009103200006201025160804008050003 2A0{total}0{seq}F0")
                .replace(' ', "")
        })
        .collect();
    for (i, p) in parts.iter().enumerate() {
        assert!(matches!(sms_pdu::decode(p), Ok(Pdu::Deliver(_))), "{p}");
        let now = r.m.now();
        r.m.t.sim.receive_sms(p, now).unwrap();
        if i == 1 {
            r.m.t.sim.receive_sms(&type_0(9), now).unwrap();
        }
        r.run(500);
    }
    assert_eq!(r.save_all(), parts);
    r.run(2000);
    assert!(r.m.t.sim.messages().is_empty());
}

#[test]
fn sim_send_short_and_long() {
    let mut r = SimRig::new(vec![]);
    let long = "dynatac test: ünïcödé ☎ 👋 — long enough to need two parts as UCS2, which allows only 67 units per part with a header.";
    let a = r.s.send(&mut r.m, NUMBER, "hello").unwrap();
    let b = r.s.send(&mut r.m, NUMBER, long).unwrap();
    r.run(10_000);
    assert_eq!(
        r.events,
        [SmsEvent::Sent { id: a, result: SendResult::Sent }, SmsEvent::Sent { id: b, result: SendResult::Sent }]
    );
    let mut want: Vec<SentSms> =
        sms_pdu::encode_submit(NUMBER, "hello", 1).unwrap().iter().map(|p| SentSms::Pdu(p.hex().into())).collect();
    want.extend(sms_pdu::encode_submit(NUMBER, long, 1).unwrap().iter().map(|p| SentSms::Pdu(p.hex().into())));
    assert_eq!(r.m.t.sim.sent_sms(), want);
}

/// A text queued just after a restart waits for PDU mode; it is never
/// sent in text mode.
#[test]
fn sim_send_across_a_restart() {
    let mut r = SimRig::new(vec![]);
    let now = r.m.now();
    r.m.t.sim.cut_power(now);
    r.m.t.sim.power_on(now);
    r.run(7_000); // `*ATREADY` seen, SMS not ready yet
    let a = r.s.send(&mut r.m, NUMBER, "hello").unwrap();
    r.run(20_000);
    assert_eq!(r.events, [SmsEvent::Sent { id: a, result: SendResult::Sent }]);
    assert!(matches!(r.m.t.sim.sent_sms(), [SentSms::Pdu(_)]));
}

// --- under faults: the real HAL over the faulty link ---

mod under_faults {
    use std::collections::BTreeMap;

    use super::*;
    use crate::modem::driver;
    use crate::modem::faults::{Faults, FaultyLink, Rng, Split};
    use crate::modem::hal::{Hal, Output};
    use crate::modem::transport::{Clock, RX_QUEUE_BYTES};

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

    /// The device's durable message storage, the network's queue of
    /// messages for the device, and the texts the user sent.
    struct Soak {
        hal: Hal,
        link: FaultyLink,
        store: SmsStore,
        rng: Rng,
        /// Messages the network has for us, not yet on the SIM.
        network: VecDeque<String>,
        next_message: u32,
        /// Every message the network delivered to the SIM.
        delivered: BTreeSet<String>,
        /// Saved by the device: PDU → when it was offered and saved.
        saved: BTreeSet<String>,
        /// Offered, the device hasn't got round to saving yet.
        to_save: Vec<(Millis, String)>,
        /// Texts sent, by id: how many parts, and the result (once known).
        texts: BTreeMap<SendId, (usize, Option<SendResult>)>,
        next_text: u32,
    }

    impl Soak {
        fn new(faults: Faults, seed: u64) -> Self {
            let mut s = Soak {
                hal: Hal::new(),
                link: FaultyLink::new(SimConfig::default(), faults, seed),
                store: SmsStore::new(seed as u8),
                rng: Rng::new(seed ^ 0x5115),
                network: VecDeque::new(),
                next_message: 0,
                delivered: BTreeSet::new(),
                saved: BTreeSet::new(),
                to_save: Vec::new(),
                texts: BTreeMap::new(),
                next_text: 0,
            };
            s.link.sim.power_on(0);
            s.hal.power_on(0);
            s
        }

        fn step(&mut self) -> Result<(), String> {
            let before = self.link.now();
            let mut out = Vec::new();
            driver::step(&mut self.hal, &mut self.link, 20, &mut out).unwrap();
            let now = self.link.now();
            self.link.roll_session_faults((now - before).max(1));
            for o in out {
                let n = match o {
                    Output::Done { id, result } => Notice::Done { id, result },
                    Output::Event(e) => Notice::Event(e),
                    Output::Write(_) => unreachable!("the driver carries out writes"),
                };
                self.store.handle(&mut At(&mut self.hal, now), n, now);
            }
            self.store.tick(&mut At(&mut self.hal, now), now);
            while let Some(e) = self.store.poll_event() {
                match e {
                    SmsEvent::Received { pdu } => {
                        if self.saved.contains(&pdu) {
                            // Offered again: only allowed if it is still on the SIM
                            // (e.g. its delete failed); the device de-duplicates.
                            continue;
                        }
                        if !self.delivered.contains(&pdu) {
                            return Err(format!("offered a message never delivered: {pdu}"));
                        }
                        if self.to_save.iter().any(|(_, p)| *p == pdu) {
                            return Err(format!("offered twice before being saved: {pdu}"));
                        }
                        // The device takes a moment to store it.
                        self.to_save.push((now + self.rng.below(3000), pdu));
                    }
                    SmsEvent::Sent { id, result } => {
                        let t = self.texts.get_mut(&id).ok_or("result for an unknown text")?;
                        if t.1.replace(result).is_some() {
                            return Err(format!("two results for {id:?}"));
                        }
                    }
                }
            }
            let (due, later): (Vec<_>, Vec<_>) = self.to_save.drain(..).partition(|(t, _)| *t <= now);
            self.to_save = later;
            for (_, pdu) in due {
                self.saved.insert(pdu.clone());
                self.store.saved(&mut At(&mut self.hal, now), &pdu, now);
            }
            self.check_sim_and_network()
        }

        /// Safety, at every step: nothing on the SIM was lost before being
        /// saved, nothing was sent in text mode, no part was sent twice.
        fn check_sim_and_network(&self) -> Result<(), String> {
            for p in &self.delivered {
                let on_sim = self.link.sim.messages().iter().any(|m| m.pdu == *p);
                if !on_sim && !self.saved.contains(p) {
                    return Err(format!("message deleted before it was saved: {p}"));
                }
            }
            let mut seen = BTreeSet::new();
            for s in self.link.sim.sent_sms() {
                match s {
                    SentSms::Text { to, body } => return Err(format!("sent in text mode to {to}: {body}")),
                    SentSms::Pdu(p) if !seen.insert(p) => return Err(format!("part sent twice: {p}")),
                    SentSms::Pdu(_) => {}
                }
            }
            Ok(())
        }

        fn workload(&mut self) {
            let now = self.link.now();
            if self.rng.chance(0.004) {
                // A message (sometimes a silent one) for us.
                let n = self.next_message;
                self.next_message += 1;
                let p = if self.rng.chance(0.1) { type_0(n) } else { pdu(n) };
                self.network.push_back(p);
            }
            // The network delivers when the SIM has room (else it retries later).
            if let Some(p) = self.network.front().cloned() {
                if self.link.sim.receive_sms(&p, now).is_some() {
                    self.network.pop_front();
                    if !is_type_0(&p) {
                        self.delivered.insert(p);
                    }
                }
            }
            if self.rng.chance(0.002) {
                let n = self.next_text;
                self.next_text += 1;
                let text = if self.rng.chance(0.3) { format!("long {n} {}", "x".repeat(170)) } else { format!("text {n}") };
                let parts = sms_pdu::encode_submit(NUMBER, &text, 0).unwrap().len();
                let id = self.store.send(&mut At(&mut self.hal, now), NUMBER, &text).unwrap();
                self.texts.insert(id, (parts, None));
            }
        }

        /// Faults off, no new traffic, `ms` of quiet: every delivered message
        /// saved and gone from the SIM, every text resolved, and every text
        /// reported sent fully on the network.
        fn check_converged(&mut self, ms: Millis) -> Result<(), String> {
            self.link.faults_on = false;
            let end = self.link.now() + ms;
            while self.link.now() < end {
                let now = self.link.now();
                if let Some(p) = self.network.front().cloned() {
                    if self.link.sim.receive_sms(&p, now).is_some() {
                        self.network.pop_front();
                        if !is_type_0(&p) {
                            self.delivered.insert(p);
                        }
                    }
                }
                self.step()?;
            }
            self.link.faults_on = true;
            let report = |what: String| {
                let recent: Vec<_> = self.link.traffic.iter().rev().take(40).collect();
                Err(format!("at {}: {what}\n{:#?}\nrecent traffic (newest first): {recent:#?}", self.link.now(), self.store))
            };
            if !self.network.is_empty() {
                return report(format!(
                    "{} messages still waiting for room on the SIM; SIM: {:?}",
                    self.network.len(),
                    self.link.sim.messages().iter().map(|m| m.index).collect::<Vec<_>>()
                ));
            }
            let unsaved: Vec<_> = self.delivered.difference(&self.saved).collect();
            if !unsaved.is_empty() {
                return report(format!("delivered but not saved: {unsaved:?}"));
            }
            if !self.link.sim.messages().is_empty() {
                return report(format!("left on the SIM: {:?}", self.link.sim.messages()));
            }
            if let Some((id, _)) = self.texts.iter().find(|(_, (_, r))| r.is_none()) {
                return report(format!("text {id:?} unresolved"));
            }
            let on_air: BTreeSet<_> = self
                .link
                .sim
                .sent_sms()
                .iter()
                .map(|s| match s {
                    SentSms::Pdu(p) => p.clone(),
                    SentSms::Text { .. } => unreachable!("checked every step"),
                })
                .collect();
            let parts_ok: usize = self.texts.values().filter(|(_, r)| *r == Some(SendResult::Sent)).map(|(p, _)| p).sum();
            if on_air.len() < parts_ok {
                return report(format!("{} texts reported sent ({parts_ok} parts), {} parts on air", self.sent(), on_air.len()));
            }
            Ok(())
        }

        fn soak(&mut self, total: Millis) -> Result<(), String> {
            let end = self.link.now() + total;
            while self.link.now() < end {
                let stretch = self.link.now() + 20_000 + self.rng.below(40_000);
                while self.link.now() < stretch {
                    self.workload();
                    self.step()?;
                }
                // A stalled command times out, the link resyncs, a reboot
                // finishes, a retry or poll lands, the SIM drains.
                self.check_converged(POLL_MS + 30_000)?;
            }
            Ok(())
        }

        fn sent(&self) -> usize {
            self.texts.values().filter(|(_, r)| *r == Some(SendResult::Sent)).count()
        }
    }

    const MIN: Millis = 60_000;

    #[test]
    fn clean_session_moves_and_sends_everything() {
        let mut s = Soak::new(Faults::none(), 1);
        s.soak(30 * MIN).unwrap();
        assert!(s.saved.len() > 50, "workload too thin: {} received", s.saved.len());
        assert!(s.sent() > 20, "workload too thin: {} sent", s.sent());
        assert_eq!(s.sent(), s.texts.len(), "without faults every text is sent");
    }

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
            // The real capacity (pauses still overflow it): a full SIM's
            // listing doesn't fit the call soak's 512 bytes, and would then be
            // lost on every attempt — see `RX_QUEUE_BYTES`.
            queue: RX_QUEUE_BYTES,
        };
        let (mut saved, mut sent) = (0, 0);
        for seed in 1..=30 {
            let mut s = Soak::new(f, seed);
            if let Err(e) = s.soak(15 * MIN) {
                panic!("seed {seed}: {e}");
            }
            saved += s.saved.len();
            sent += s.sent();
        }
        assert!(saved > 500 && sent > 100, "workload too thin: {saved} received, {sent} sent");
    }

    #[test]
    fn converges_after_heavy_loss_and_restarts() {
        let f = Faults { split: Split::Random, drop: 0.05, pause: 0.05, restart: 0.01, queue: RX_QUEUE_BYTES, ..Faults::none() };
        for seed in 100..115 {
            let mut s = Soak::new(f, seed);
            if let Err(e) = s.soak(15 * MIN) {
                panic!("seed {seed}: {e}");
            }
        }
    }
}
