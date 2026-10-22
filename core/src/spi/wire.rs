//! L0 of the SPI stack: the raw pins and bytes of the bus.
//!
//! [`Wire`] is the whole of what the device has to implement for the SPI
//! stack: drive an output pin, read an input pin, clock bytes at a given
//! speed, pause a few microseconds. It knows nothing of devices, chip
//! selects or protocols: the [`bus`](super::bus) layer adds those, so the
//! rules are enforced (and tested) in one place, on the host.
//!
//! Implementations:
//!   - the device: ESP-IDF `spi_master` on SPI2 plus GPIO drivers (later);
//!   - `tests::MockWire` (test builds only): a wire-level model of the
//!     board that panics on any violation of the bus rules.
//!
//! Callee invariants (every implementation):
//!   - `transfer` clocks exactly `out.len()` bytes in SPI mode 0, MSB
//!     first, at no more than `clock`, and touches no pin.
//!   - When `input` is given, it receives what was on MISO, byte for byte.
//!   - On `WireError` an unknown prefix of `out` may have been clocked.
//!
//! Caller invariants:
//!   - `input`, when given, has the same length as `out`.
//!   - `pause_us` is only used for pauses of a few microseconds (it may
//!     busy-wait).

/// Pins the bus layer drives. Board wiring: `docs/spi-research.md` §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Output {
    /// E-paper chip select (GPIO34), active low.
    EpdCs,
    /// E-paper data/command (GPIO35): low = command, high = data.
    EpdDc,
    /// SD card chip select (GPIO48), active low.
    SdCs,
    /// LoRa chip select (GPIO3), active low.
    LoraCs,
    /// LoRa reset (GPIO4), active low.
    LoraRst,
    /// LoRa supply enable (GPIO46), high = powered.
    LoraEn,
}

/// Pins the bus layer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Input {
    /// E-paper BUSY_N (GPIO37): low = busy.
    EpdBusy,
    /// LoRa BUSY (GPIO6): high = busy.
    LoraBusy,
    /// LoRa DIO1 interrupt (GPIO5).
    LoraDio1,
}

/// An SPI clock frequency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hz(pub u32);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

/// A monotonic clock in milliseconds. The spi task reads it afresh as it
/// works, so waits expire on time even while other devices use the bus.
pub trait Clock {
    fn now(&self) -> crate::spi::Millis;
}

pub trait Wire {
    fn set(&mut self, pin: Output, high: bool);
    fn get(&mut self, pin: Input) -> bool;
    /// Clock `out` at `clock`, capturing MISO into `input` if given.
    fn transfer(&mut self, clock: Hz, out: &[u8], input: Option<&mut [u8]>) -> Result<(), WireError>;
    fn pause_us(&mut self, us: u32);
}

/// Test support: [`MockWire`](tests::MockWire) and the simulated devices
/// it hosts.
#[cfg(test)]
pub(crate) mod tests;
