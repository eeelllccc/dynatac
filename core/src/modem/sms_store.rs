//! L3 SMS service: moves received messages off the SIM, and sends texts.
//!
//! [`SmsStore`] is a pure state machine written against [`ModemPort`],
//! used like `CallManager`: the owner polls the port and hands *every*
//! notice to [`SmsStore::handle`] (`Done`s for other services' requests are
//! ignored), and calls [`SmsStore::tick`] regularly.
//!
//! Receiving. The SIM holds 10 messages, so they are moved to the device:
//! each one is offered as [`SmsEvent::Received`] (its raw PDU), the owner
//! stores it durably and calls [`SmsStore::saved`], and only then is it
//! deleted from the SIM. Losing power at any point can't lose a message,
//! only offer it again (after a restart of the device): the PDU is the
//! key for de-duplicating. The modem's listing is the truth; `+CMTI` is a
//! hint. Work happens in *rounds*:
//!   1. list every message on the SIM (`AT+CMGL=4`);
//!   2. offer each new one; for each one the owner has saved (or a Type 0
//!      "silent" message, which is never offered), delete its slot.
//!
//! A slot is only deleted on the evidence of the listing just taken, and
//! at most once per round. Only this store deletes, so the slot still
//! holds that message; a slot freed by a delete may at once be reused by a
//! new arrival, which a blind retry of the delete would destroy — so a
//! failed or unknown delete is only retried after a fresh listing.
//! A round runs on every `Configured`, `+CMTI`, `SMS DONE`, lost bytes and
//! save; a hint arriving mid-round runs another after it; a failed round
//! is retried after [`RETRY_MS`]; and one runs every [`POLL_MS`] anyway,
//! in case a `+CMTI` was lost unseen.
//!
//! Sending. [`SmsStore::send`] encodes the text (one or more parts) and
//! queues it; queued texts go out one part at a time, in order, while the
//! link is configured. Each text ends with exactly one [`SmsEvent::Sent`]:
//!   - `NotSent` (the modem never saw it, e.g. PDU mode not yet confirmed
//!     after a restart): retried after the next `Configured`, up to
//!     [`MAX_ATTEMPTS`] times per part;
//!   - refused (`+CMS ERROR`): failed, the remaining parts are dropped;
//!   - unknown (timeout, lost bytes, restart mid-send): reported, never
//!     retried — the part may have gone out, and a duplicate costs money.
//!
//! Invariants (callee):
//!   - Only slots a listing in the current round showed holding a saved
//!     (or Type 0) message are deleted; each at most once per round.
//!   - A message is offered at most once per `SmsStore` while it stays
//!     on the SIM unsaved.
//!   - At most one listing and one send part in flight; every `send` gets
//!     exactly one `Sent` event; no part is submitted twice unless the
//!     earlier attempt was `NotSent`.
//!
//! Caller invariants:
//!   - Every notice polled from the port is passed to [`SmsStore::handle`],
//!     in order; the same port is passed to every method.
//!   - `now` never decreases; [`SmsStore::tick`] is called every second or so.
//!   - [`SmsStore::saved`] only after the message is durably stored.

use std::collections::{BTreeSet, VecDeque};

use super::hal::{Event, RequestError, RequestId};
use super::link::{Failure, Millis};
use super::port::{ModemPort, Notice};
use super::request::{Request, Response};
use super::sms_pdu::{self, EncodeError, Pdu, SubmitPdu, PID_TYPE_0};
use super::unsolicited::Unsolicited;

/// A round runs at least this often, in case a `+CMTI` was lost unseen.
pub const POLL_MS: Millis = 60_000;
/// A round that failed (listing or delete) is retried after this.
pub const RETRY_MS: Millis = 10_000;
/// A part refused with `NotSent` is retried this many times in all.
pub const MAX_ATTEMPTS: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SendId(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmsEvent {
    /// A received message (or one part of one), as a PDU for
    /// `sms_pdu::decode`. Store it, then call [`SmsStore::saved`].
    Received { pdu: String },
    /// A text from [`SmsStore::send`] is finished with.
    Sent { id: SendId, result: SendResult },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendResult {
    /// Every part was accepted by the network.
    Sent,
    /// Part `parts_sent + 1` was not sent; the earlier ones were.
    Failed { parts_sent: usize, error: SendError },
    /// Part `parts_sent + 1` may or may not have gone out; nothing after it
    /// was sent.
    Unknown { parts_sent: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The modem or network refused it.
    Refused(Failure),
    /// Never sent: the modem wasn't ready, [`MAX_ATTEMPTS`] times.
    NotSent,
}

#[derive(Debug)]
struct Outgoing {
    id: SendId,
    parts: Vec<SubmitPdu>,
    /// Parts accepted so far.
    sent: usize,
    /// `NotSent` results for the current part.
    attempts: u8,
}

#[derive(Debug, PartialEq, Eq)]
enum Round {
    Idle,
    Listing(RequestId),
    Deleting(Vec<RequestId>),
}

#[derive(Debug)]
pub struct SmsStore {
    round: Round,
    /// A hint arrived during the current round (or while the link was
    /// down): another round is due.
    again: bool,
    /// Something in the current round failed: retry after `RETRY_MS`.
    failed: bool,
    /// When the next round is due by itself (poll or retry).
    due: Option<Millis>,
    /// Offered, not yet saved.
    offered: BTreeSet<String>,
    /// Saved by the owner, still on the SIM as of the last listing.
    saved: BTreeSet<String>,
    events: VecDeque<SmsEvent>,
    outbox: VecDeque<Outgoing>,
    /// The part in flight (of `outbox.front()`).
    sending: Option<RequestId>,
    /// A part came back `NotSent`: wait for the next `Configured`.
    send_blocked: bool,
    next_id: u32,
    next_reference: u8,
    link_up: bool,
    sim_used: Option<usize>,
}

impl SmsStore {
    /// `reference_seed`: the first multipart reference number. Vary it per
    /// boot (any source will do) so parts of texts sent just before and
    /// after a restart aren't joined by the recipient.
    pub fn new(reference_seed: u8) -> Self {
        SmsStore {
            round: Round::Idle,
            again: false,
            failed: false,
            due: None,
            offered: BTreeSet::new(),
            saved: BTreeSet::new(),
            events: VecDeque::new(),
            outbox: VecDeque::new(),
            sending: None,
            send_blocked: false,
            next_id: 0,
            next_reference: reference_seed,
            link_up: false,
            sim_used: None,
        }
    }

    /// The next thing to report, oldest first.
    pub fn poll_event(&mut self) -> Option<SmsEvent> {
        self.events.pop_front()
    }

    /// Messages on the SIM at the last listing (10 fit).
    pub fn sim_used(&self) -> Option<usize> {
        self.sim_used
    }

    /// Texts queued or being sent: `(id, parts sent, parts)`.
    pub fn outbox(&self) -> impl Iterator<Item = (SendId, usize, usize)> + '_ {
        self.outbox.iter().map(|o| (o.id, o.sent, o.parts.len()))
    }

    /// The owner has durably stored `pdu` (from [`SmsEvent::Received`]): it
    /// may now be deleted from the SIM.
    pub fn saved<P: ModemPort + ?Sized>(&mut self, port: &mut P, pdu: &str, now: Millis) {
        if self.offered.remove(pdu) {
            self.saved.insert(pdu.to_string());
            self.want_round(port, now);
        }
    }

    /// Queue a text to `number`. Fails only if it can't be encoded.
    pub fn send<P: ModemPort + ?Sized>(&mut self, port: &mut P, number: &str, text: &str) -> Result<SendId, EncodeError> {
        let parts = sms_pdu::encode_submit(number, text, self.next_reference)?;
        if parts.len() > 1 {
            self.next_reference = self.next_reference.wrapping_add(1);
        }
        let id = SendId(self.next_id);
        self.next_id += 1;
        self.outbox.push_back(Outgoing { id, parts, sent: 0, attempts: 0 });
        self.pump_outbox(port);
        Ok(id)
    }

    /// Start a round if one is due (poll or retry).
    pub fn tick<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        if self.link_up && self.round == Round::Idle && self.due.is_some_and(|d| now >= d) {
            self.start_round(port, now);
        }
    }

    pub fn handle<P: ModemPort + ?Sized>(&mut self, port: &mut P, notice: Notice, now: Millis) {
        match notice {
            Notice::Done { id, result } => self.on_done(port, id, result, now),
            Notice::Event(e) => self.on_event(port, e, now),
        }
    }

    fn on_event<P: ModemPort + ?Sized>(&mut self, port: &mut P, event: Event, now: Millis) {
        match event {
            Event::Configured { .. } => {
                self.link_up = true;
                self.send_blocked = false;
                self.want_round(port, now);
                self.pump_outbox(port);
            }
            Event::LinkLost(_) | Event::Restarted => self.link_up = false,
            Event::Urc(Unsolicited::NewSms { .. } | Unsolicited::SmsReady) | Event::BytesLost => {
                self.want_round(port, now)
            }
            Event::Urc(_) | Event::UnexpectedPrompt => {}
        }
    }

    fn on_done<P: ModemPort + ?Sized>(
        &mut self,
        port: &mut P,
        id: RequestId,
        result: Result<Response, RequestError>,
        now: Millis,
    ) {
        if self.sending == Some(id) {
            self.sending = None;
            self.on_sent(result);
            self.pump_outbox(port);
        } else if self.round == Round::Listing(id) {
            match result {
                Ok(Response::Messages(list)) => self.on_listing(port, list, now),
                _ => {
                    self.failed = true;
                    self.end_round(port, now);
                }
            }
        } else if let Round::Deleting(ids) = &mut self.round {
            if let Some(i) = ids.iter().position(|d| *d == id) {
                ids.remove(i);
                self.failed |= !matches!(result, Ok(Response::Done));
                if ids.is_empty() {
                    self.end_round(port, now);
                }
            }
        }
    }

    fn on_listing<P: ModemPort + ?Sized>(
        &mut self,
        port: &mut P,
        list: Vec<super::sms_storage::Stored>,
        now: Millis,
    ) {
        self.sim_used = Some(list.len());
        let on_sim: BTreeSet<&str> = list.iter().map(|m| m.pdu.as_str()).collect();
        // Forget what has left the SIM.
        self.saved.retain(|p| on_sim.contains(p.as_str()));
        self.offered.retain(|p| on_sim.contains(p.as_str()));
        let mut deletes = Vec::new();
        for m in &list {
            if self.saved.contains(&m.pdu) || is_type_0(&m.pdu) {
                deletes.push(port.submit(Request::DeleteMessage(m.index)));
            } else if self.offered.insert(m.pdu.clone()) {
                self.events.push_back(SmsEvent::Received { pdu: m.pdu.clone() });
            }
        }
        if deletes.is_empty() {
            self.end_round(port, now);
        } else {
            self.round = Round::Deleting(deletes);
        }
    }

    fn end_round<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        self.round = Round::Idle;
        if std::mem::take(&mut self.failed) {
            self.again = false;
            self.due = Some(now + RETRY_MS);
        } else if self.again && self.link_up {
            self.start_round(port, now);
        } else {
            self.due = Some(now + POLL_MS);
        }
    }

    fn want_round<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        if self.round == Round::Idle && self.link_up {
            self.start_round(port, now);
        } else {
            // Mid-round, or the link is down: `end_round` / `Configured` runs it.
            self.again = true;
        }
    }

    fn start_round<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        self.again = false;
        self.failed = false;
        self.due = Some(now + POLL_MS);
        self.round = Round::Listing(port.submit(Request::ListMessages));
    }

    fn on_sent(&mut self, result: Result<Response, RequestError>) {
        let Some(o) = self.outbox.front_mut() else { return };
        let finished = match result {
            // `OK` (even with an odd reply line) means the network took it.
            Ok(_) | Err(RequestError::Malformed(_)) => {
                o.sent += 1;
                o.attempts = 0;
                (o.sent == o.parts.len()).then_some(SendResult::Sent)
            }
            Err(RequestError::NotSent) => {
                o.attempts += 1;
                self.send_blocked = true;
                (o.attempts >= MAX_ATTEMPTS)
                    .then_some(SendResult::Failed { parts_sent: o.sent, error: SendError::NotSent })
            }
            Err(RequestError::Failed(f)) => Some(SendResult::Failed { parts_sent: o.sent, error: SendError::Refused(f) }),
            Err(RequestError::Unknown(_)) => Some(SendResult::Unknown { parts_sent: o.sent }),
        };
        if let Some(result) = finished {
            let o = self.outbox.pop_front().expect("front exists");
            self.events.push_back(SmsEvent::Sent { id: o.id, result });
        }
    }

    fn pump_outbox<P: ModemPort + ?Sized>(&mut self, port: &mut P) {
        if self.sending.is_some() || !self.link_up || self.send_blocked {
            return;
        }
        if let Some(o) = self.outbox.front() {
            self.sending = Some(port.submit(Request::SendMessage(o.parts[o.sent].clone())));
        }
    }
}

/// A "silent" Type 0 message: acknowledged by the modem, never shown.
fn is_type_0(pdu: &str) -> bool {
    matches!(sms_pdu::decode(pdu), Ok(Pdu::Deliver(d)) if d.pid == PID_TYPE_0)
}

#[cfg(test)]
mod tests;
