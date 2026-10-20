//! L3 call service: the state of the voice call, kept true by querying.
//!
//! [`CallManager`] is a pure state machine written against [`ModemPort`].
//! It does not own the port: the owner polls the port and hands *every*
//! notice to [`CallManager::handle`] (other services may share the port;
//! `Done`s for requests this manager didn't submit are ignored). User
//! actions ([`dial`](CallManager::dial), [`answer`](CallManager::answer),
//! [`hang_up`](CallManager::hang_up), [`set_volume`](CallManager::set_volume))
//! and [`tick`](CallManager::tick) submit requests on the port they're given.
//!
//! Truth is the modem's call list (`AT+CLCC`); everything else is a hint:
//!   - a `+CLCC` URC updates the list at once (same format as the query);
//!   - other call URCs (`RING`, `NO CARRIER`, `VOICE CALL: END` …), lost
//!     bytes, link loss and every `Configured` trigger a query;
//!   - a query reply is applied only if no hint arrived while it was in
//!     flight; otherwise it may predate the hint, so it is discarded and the
//!     query re-sent (a `+CLCC` absorbed into a reply, or `NO CARRIER`
//!     landing mid-reply, can make a reply stale);
//!   - while a call is tracked, or until a query has succeeded, the list is
//!     re-queried every [`POLL_MS`];
//!   - `Restarted` empties the list (calls die with the modem).
//!
//! User actions are refused ([`Refused::NotReady`]) from `LinkLost` /
//! `Restarted` until the next `Configured`, and before the first one: the
//! HAL would queue them until the link is back, maybe many seconds later,
//! when an `ATA` or `AT+CHUP` could hit a different call than the one the
//! user saw (found by the fault soak: a queued `ATA` sent after the caller
//! had gone).
//!
//! One call is tracked: the first voice call to appear (lowest index).
//! Further calls (a waiting call) are not shown; when the tracked call ends
//! the next one is picked up. Non-voice calls are ignored.
//!
//! Invariants (callee):
//!   - [`CallManager::status`] reflects the last applied list, plus a dial
//!     not yet seen in it. A tracked call that leaves the list becomes
//!     [`CallStatus::Ended`] exactly once.
//!   - At most one `ListCalls`, one `ATA` and one `AT+CHUP` in flight
//!     (repeated actions while one is in flight are accepted, not resent).
//!   - The wanted volume is re-applied after every `Configured` (the HAL's
//!     configuration resets the gain to maximum).
//!
//! Caller invariants:
//!   - Every notice polled from the port is passed to [`CallManager::handle`],
//!     in order; the same port is passed to every method.
//!   - `now` never decreases.
//!   - [`CallManager::tick`] is called at least every [`POLL_MS`] or so.

use super::hal::{Event, RequestError, RequestId};
use super::link::Millis;
use super::port::{ModemPort, Notice};
use super::request::{Gain, PhoneNumber, Request, Response};
use super::unsolicited::{CallInfo, CallMode, CallState, Direction, Unsolicited};

/// How often the call list is re-queried while a call is tracked (and
/// retried after a failed query).
pub const POLL_MS: Millis = 2_000;

/// What the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallStatus {
    Idle,
    /// We dialled; the far end isn't ringing yet.
    Dialling { number: String },
    /// The far end is ringing.
    Alerting { number: String },
    /// Connected (or on hold) since `since`.
    Active { number: Option<String>, since: Millis },
    /// Someone is calling us.
    Incoming { number: Option<String> },
    /// The last call ended; shown until [`CallManager::dismiss`] or a new call.
    Ended(Ended),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub direction: Direction,
    pub number: Option<String>,
    /// How long the call was connected; `None` if it never was (a missed
    /// incoming call, or an outgoing call that failed or wasn't answered).
    pub connected_ms: Option<Millis>,
}

/// Why an action was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The modem link is down (not yet configured, resyncing, rebooting).
    NotReady,
    /// There is already a call (or a dial in progress).
    Busy,
    /// There is no call to act on.
    NoCall,
    /// The call is not ringing in.
    NotIncoming,
}

/// The call being shown.
#[derive(Debug, Clone)]
struct Tracked {
    /// `None` while our dial hasn't appeared in the list yet.
    index: Option<u8>,
    direction: Direction,
    number: Option<String>,
    connected_at: Option<Millis>,
}

/// A dial that hasn't appeared in the call list yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dial {
    /// `ATD` not answered yet.
    Sent(RequestId),
    /// `ATD` answered (or its outcome is unknown): the modem started the
    /// call if it ever will. A list taken from now on is conclusive.
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Answer,
    HangUp,
}

#[derive(Debug)]
pub struct CallManager {
    /// Voice calls, as last known (no `Disconnected` entries).
    calls: Vec<CallInfo>,
    tracked: Option<Tracked>,
    dial: Option<Dial>,
    ended: Option<Ended>,
    /// The `ListCalls` in flight.
    query: Option<RequestId>,
    /// A hint arrived while `query` was in flight: its reply may be stale.
    requery: bool,
    /// When the last `ListCalls` was submitted.
    last_query: Option<Millis>,
    /// The last `ListCalls` succeeded (the list is not known to be stale).
    synced: bool,
    actions: Vec<(RequestId, Action)>,
    volume: Gain,
    /// Configured since the last `LinkLost` / `Restarted`: actions go out now.
    link_up: bool,
}

impl Default for CallManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CallManager {
    /// Nothing known yet: the first [`tick`](Self::tick) queries; actions
    /// wait for the first `Configured`.
    pub fn new() -> Self {
        CallManager {
            calls: Vec::new(),
            tracked: None,
            dial: None,
            ended: None,
            query: None,
            requery: false,
            last_query: None,
            synced: false,
            actions: Vec::new(),
            volume: Gain::MAX,
            link_up: false,
        }
    }

    pub fn status(&self) -> CallStatus {
        let Some(t) = &self.tracked else {
            return self.ended.clone().map_or(CallStatus::Idle, CallStatus::Ended);
        };
        let state = t.index.and_then(|i| self.call(i)).map(|c| c.state);
        let number = t.number.clone();
        match state {
            None | Some(CallState::Dialling) => CallStatus::Dialling { number: number.unwrap_or_default() },
            Some(CallState::Alerting) => CallStatus::Alerting { number: number.unwrap_or_default() },
            Some(CallState::Active | CallState::Held) => {
                CallStatus::Active { number, since: t.connected_at.unwrap_or_default() }
            }
            Some(CallState::Incoming | CallState::Waiting) => CallStatus::Incoming { number },
            Some(CallState::Disconnected) => unreachable!("disconnected calls leave the list"),
        }
    }

    /// The volume the user wants (applied to the modem when possible).
    pub fn volume(&self) -> Gain {
        self.volume
    }

    /// Forget the last ended call (status goes back to `Idle`).
    pub fn dismiss(&mut self) {
        self.ended = None;
    }

    pub fn dial<P: ModemPort + ?Sized>(&mut self, port: &mut P, number: PhoneNumber, now: Millis) -> Result<(), Refused> {
        if self.tracked.is_some() || !self.calls.is_empty() {
            return Err(Refused::Busy);
        }
        if !self.link_up {
            return Err(Refused::NotReady);
        }
        let id = port.submit(Request::Dial(number.clone()));
        self.dial = Some(Dial::Sent(id));
        self.ended = None;
        self.tracked = Some(Tracked {
            index: None,
            direction: Direction::Outgoing,
            number: Some(number.as_str().to_string()),
            connected_at: None,
        });
        // A poll during dialling is due from now, not from the last idle query.
        self.last_query = Some(now);
        Ok(())
    }

    pub fn answer<P: ModemPort + ?Sized>(&mut self, port: &mut P) -> Result<(), Refused> {
        match self.status() {
            CallStatus::Incoming { .. } if !self.link_up => Err(Refused::NotReady),
            CallStatus::Incoming { .. } => {
                self.act(port, Action::Answer);
                Ok(())
            }
            _ if self.tracked.is_some() => Err(Refused::NotIncoming),
            _ => Err(Refused::NoCall),
        }
    }

    /// End the call (`AT+CHUP` ends every call). Also rejects an incoming
    /// call and cancels a dial.
    pub fn hang_up<P: ModemPort + ?Sized>(&mut self, port: &mut P) -> Result<(), Refused> {
        if self.tracked.is_none() {
            return Err(Refused::NoCall);
        }
        if !self.link_up {
            return Err(Refused::NotReady);
        }
        self.act(port, Action::HangUp);
        Ok(())
    }

    /// Submit `action` unless the same one is already in flight: a repeated
    /// key press must not queue a second `ATA`, which with no call left
    /// stalls the link until its timeout (found by the fault soak).
    fn act<P: ModemPort + ?Sized>(&mut self, port: &mut P, action: Action) {
        if self.actions.iter().any(|(_, a)| *a == action) {
            return;
        }
        let request = match action {
            Action::Answer => Request::Answer,
            Action::HangUp => Request::HangUp,
        };
        self.actions.push((port.submit(request), action));
    }

    /// Applied now, or when the link is next configured.
    pub fn set_volume<P: ModemPort + ?Sized>(&mut self, port: &mut P, gain: Gain) {
        self.volume = gain;
        if self.link_up {
            port.submit(Request::SetOutputGain(gain));
        }
    }

    /// Whether actions are accepted (the link is configured).
    pub fn is_ready(&self) -> bool {
        self.link_up
    }

    /// Periodic re-query: while a call is tracked, or until a query succeeds.
    pub fn tick<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        let due = self.last_query.is_none_or(|t| now >= t + POLL_MS);
        if self.query.is_none() && due && (self.tracked.is_some() || !self.synced) {
            self.submit_query(port, now);
        }
    }

    pub fn handle<P: ModemPort + ?Sized>(&mut self, port: &mut P, notice: Notice, now: Millis) {
        match notice {
            Notice::Done { id, result } => self.on_done(port, id, result, now),
            Notice::Event(e) => self.on_event(port, e, now),
        }
    }

    fn on_done<P: ModemPort + ?Sized>(
        &mut self,
        port: &mut P,
        id: RequestId,
        result: Result<Response, RequestError>,
        now: Millis,
    ) {
        if self.query == Some(id) {
            self.query = None;
            if std::mem::take(&mut self.requery) {
                self.submit_query(port, now);
                return;
            }
            match result {
                Ok(Response::Calls(list)) => {
                    self.synced = true;
                    self.calls = list.into_iter().filter(is_live_voice).collect();
                    self.reconcile(true, now);
                }
                _ => self.synced = false,
            }
        } else if self.dial == Some(Dial::Sent(id)) {
            match result {
                Ok(_) | Err(RequestError::Unknown(_) | RequestError::Malformed(_)) => {
                    self.dial = Some(Dial::Settled);
                    self.want_query(port, now);
                }
                Err(RequestError::Failed(_) | RequestError::NotSent) => {
                    self.dial = None;
                    self.reconcile(false, now);
                }
            }
        } else if let Some(i) = self.actions.iter().position(|(a, _)| *a == id) {
            let (_, action) = self.actions.remove(i);
            // Hang-up: confirm the call is gone. Answer: an `OK` is followed
            // by `+CLCC` URCs; anything else (e.g. `Unknown` because the
            // caller gave up) needs the list.
            if action == Action::HangUp || result.is_err() {
                self.want_query(port, now);
            }
        }
    }

    fn on_event<P: ModemPort + ?Sized>(&mut self, port: &mut P, event: Event, now: Millis) {
        use Unsolicited as U;
        match event {
            Event::Urc(U::Call(info)) => {
                if info.mode != CallMode::Voice {
                    return;
                }
                self.requery |= self.query.is_some();
                self.calls.retain(|c| c.index != info.index);
                if is_live_voice(&info) {
                    self.calls.push(info);
                    self.calls.sort_by_key(|c| c.index);
                }
                self.reconcile(false, now);
            }
            Event::Urc(
                U::Ring
                | U::CallerId { .. }
                | U::ConnectedLine { .. }
                | U::VoiceCallBegin
                | U::VoiceCallEnd { .. }
                | U::NoCarrier
                | U::Busy
                | U::NoAnswer,
            )
            | Event::BytesLost => self.want_query(port, now),
            Event::LinkLost(_) => {
                self.link_up = false;
                self.want_query(port, now);
            }
            Event::Restarted => {
                self.link_up = false;
                self.calls.clear();
                self.dial = None;
                self.reconcile(false, now);
                self.want_query(port, now);
            }
            Event::Configured { .. } => {
                self.link_up = true;
                if self.volume != Gain::MAX {
                    port.submit(Request::SetOutputGain(self.volume));
                }
                self.want_query(port, now);
            }
            Event::Urc(_) | Event::UnexpectedPrompt => {}
        }
    }

    fn want_query<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        if self.query.is_some() {
            self.requery = true;
        } else {
            self.submit_query(port, now);
        }
    }

    fn submit_query<P: ModemPort + ?Sized>(&mut self, port: &mut P, now: Millis) {
        self.query = Some(port.submit(Request::ListCalls));
        self.last_query = Some(now);
    }

    fn call(&self, index: u8) -> Option<&CallInfo> {
        self.calls.iter().find(|c| c.index == index)
    }

    /// Bring `tracked` in line with `calls`. `from_query`: `calls` is a full
    /// list from `AT+CLCC` (not one URC's update).
    fn reconcile(&mut self, from_query: bool, now: Millis) {
        if let Some(t) = &mut self.tracked {
            match t.index {
                // Our dial: adopt the outgoing call once it shows up.
                None => {
                    if let Some(c) = self.calls.iter().find(|c| c.direction == Direction::Outgoing) {
                        t.index = Some(c.index);
                        self.dial = None;
                    } else if self.dial.is_none() || (from_query && self.dial == Some(Dial::Settled)) {
                        self.end(now);
                    }
                }
                Some(i) => {
                    if !self.calls.iter().any(|c| c.index == i) {
                        self.end(now);
                    }
                }
            }
        }
        if self.tracked.is_none() {
            if let Some(c) = self.calls.first() {
                self.ended = None;
                self.tracked = Some(Tracked {
                    index: Some(c.index),
                    direction: c.direction,
                    number: c.number.clone(),
                    connected_at: None,
                });
            }
        }
        if let Some(t) = &mut self.tracked {
            if let Some(c) = t.index.and_then(|i| self.calls.iter().find(|c| c.index == i)) {
                if c.number.is_some() {
                    t.number = c.number.clone();
                }
                if t.connected_at.is_none() && matches!(c.state, CallState::Active | CallState::Held) {
                    t.connected_at = Some(now);
                }
            }
        }
    }

    fn end(&mut self, now: Millis) {
        if let Some(t) = self.tracked.take() {
            self.dial = None;
            self.ended = Some(Ended {
                direction: t.direction,
                number: t.number,
                connected_ms: t.connected_at.map(|at| now - at),
            });
        }
    }
}

fn is_live_voice(c: &CallInfo) -> bool {
    c.mode == CallMode::Voice && c.state != CallState::Disconnected
}

#[cfg(test)]
mod tests;
