//! The SPI2 bus and the devices on it: e-paper, SD card, LoRa radio.
//!
//! One task owns the bus (see `docs/os-design.md`). Its logic lives here,
//! pure and host-tested; the device side only implements [`wire::Wire`]:
//! set a pin, read a pin, clock some bytes.
//!
//! Layers (see `docs/spi-design.md`):
//!   - [`wire`]: raw pins and bytes, the only thing the device implements.
//!   - [`bus`]: runs one transaction at a time over a `Wire`, enforcing the
//!     rules the hardware imposes on a shared bus (B1–B5 in [`bus`]).
//!   - [`protocol`]: the interface every device protocol has (what it wants
//!     next, how that went); [`epaper`] is the first.
//!   - [`scheduler`]: [`scheduler::Spi`], the spi task's logic: requests in,
//!     round-robin transactions, responses out.
//!
//! Hardware evidence for every rule: `docs/spi-research.md`.

pub mod bus;
pub mod epaper;
pub mod protocol;
pub mod scheduler;
pub mod wire;

/// Milliseconds on the caller's monotonic clock.
pub type Millis = u64;
