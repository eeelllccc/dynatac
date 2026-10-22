//! What every device protocol on the bus has in common: it never touches
//! the bus, it only says what it wants next ([`Next`]) and is told how that
//! went ([`Outcome`]). The scheduler does the rest.
//!
//! Contract between a protocol and its driver (the scheduler, or a test):
//!   - The driver calls `next(now)` and acts on the answer: runs the
//!     transaction, or waits for the pin level / the time.
//!   - It then reports exactly one [`Outcome`] for that answer before
//!     calling `next` again (none for `Idle`).
//!   - `WaitPin` ends `Waited` once the pin reads the level, or `TimedOut`
//!     once `now >= deadline` without it. `WaitUntil(t)` ends `Waited` once
//!     `now >= t`.
//!   - `now` never decreases.

use super::bus::{BusError, Transaction};
use super::wire::Input;
use super::Millis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Nothing to do until a new request.
    Idle,
    /// Ready to run now.
    Transaction(Transaction),
    /// Waiting for `pin` to read `high`, until `deadline`.
    WaitPin { pin: Input, high: bool, deadline: Millis },
    /// Waiting until a time.
    WaitUntil(Millis),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The transaction ran (or failed) on the bus.
    Tx(Result<Vec<u8>, BusError>),
    /// The wait ended as hoped.
    Waited,
    /// A `WaitPin` reached its deadline.
    TimedOut,
}
