//! The service-facing side of the modem: what L3 (call manager, SMS store)
//! talks to. Requests in, responses and events out; bytes, time and the
//! link are hidden underneath.
//!
//! Implementations:
//!   - `tests::SimModem` (test builds only): the real HAL on the
//!     byte-level simulator.
//!   - `tests::MockHal` (test builds only): scripted, for precise control
//!     of replies and failures in service tests.
//!   - the device handle (channels to the driver thread), later.
//!
//! Contract (every implementation):
//!   - Every submitted request produces exactly one [`Notice::Done`].
//!   - Requests are carried out in submission order, one at a time.
//!   - `poll` never blocks; `None` means nothing is ready yet.
//!   - Events may arrive before, between or after responses, in modem order
//!     (e.g. `NO CARRIER` before `AT+CHUP`'s response).

use super::hal::{Event, RequestError, RequestId};
use super::request::{Request, Response};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    Done { id: RequestId, result: Result<Response, RequestError> },
    Event(Event),
}

pub trait ModemPort {
    fn submit(&mut self, request: Request) -> RequestId;
    fn poll(&mut self) -> Option<Notice>;
}

/// Test support for code written against [`ModemPort`]: [`MockHal`],
/// [`SimModem`] and the contract tests that keep the mock faithful.
#[cfg(test)]
pub(crate) mod tests {
    mod contract;
    mod mock;
    mod sim_modem;

    pub(crate) use mock::MockHal;
    pub(crate) use sim_modem::SimModem;
}
