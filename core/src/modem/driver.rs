//! The loop that connects the HAL to a transport. Shared by the device
//! (UART transport, real clock) and tests (simulator, simulated clock), so
//! tests exercise the same driving code the device runs.

use std::time::Duration;

use super::hal::{Hal, Output};
use super::link::Millis;
use super::transport::{Clock, Transport, WriteError};

/// Carry out the HAL's pending writes; collect everything else into `out`.
///
/// On a write error the rest of the HAL's output is still collected and
/// the error returned. The HAL is not told: whatever was being sent will
/// time out and the link resyncs, so the error only needs logging.
pub fn flush<T: Transport>(hal: &mut Hal, t: &mut T, out: &mut Vec<Output>) -> Result<(), WriteError> {
    let mut result = Ok(());
    while let Some(o) = hal.poll() {
        match o {
            Output::Write(b) => {
                if let Err(e) = t.write(&b) {
                    result = result.and(Err(e));
                }
            }
            other => out.push(other),
        }
    }
    result
}

/// One iteration: flush, wait for input (at most `max_wait`, and never
/// past the HAL's next deadline), feed it to the HAL, flush again.
pub fn step<T: Transport + Clock>(
    hal: &mut Hal,
    t: &mut T,
    max_wait: Millis,
    out: &mut Vec<Output>,
) -> Result<(), WriteError> {
    let first = flush(hal, t, out);
    let now = t.now();
    let wait = hal.next_deadline().map_or(max_wait, |d| d.saturating_sub(now).min(max_wait));
    match t.read(Duration::from_millis(wait)) {
        Some(rx) => hal.on_rx(rx, t.now()),
        None => hal.on_tick(t.now()),
    }
    first.and(flush(hal, t, out))
}
