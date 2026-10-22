//! The shared SPI bus: one transaction at a time, with the bus rules.
//!
//! A [`Transaction`] is data: a device, a clock and a list of parts. The
//! [`Bus`] carries it out over a [`Wire`]: chip select low, the parts,
//! chip select high, then one release byte with every chip select high.
//! It also owns the bring-up sequence that keeps the devices from
//! disturbing each other.
//!
//! Bus invariants (callee; `wire::tests::MockWire` checks each one):
//!   - B1 At most one active device has its chip select low, and only
//!     inside a transaction.
//!   - B2 Every transaction ends with every chip select high and one byte
//!     clocked, before any chip select goes low again. (An SD card keeps
//!     driving MISO after its CS goes high until it sees more clocks.)
//!   - B3 A transaction's clock is within its device's limit
//!     ([`Device::max_clock`]).
//!   - B4 Before the first transaction: ≥ 74 clocks at ≤ 400 kHz with
//!     every chip select and MOSI high, then CMD0 to the SD card, card or
//!     no card. (At power-up a card is in SD mode and doesn't stay off the
//!     bus just because its CS is high; CMD0 puts it into SPI mode.)
//!   - B5 No clocks while the LoRa supply is off. An unpowered SX1262
//!     still sits on SCK and MOSI and stops the e-paper hearing anything
//!     (measured: `device/examples/epd_probe.rs`, docs/spi-design.md
//!     build notes). So the module is powered, deselected and out of
//!     reset from bring-up on, and put to sleep (~160 nA) until the LoRa
//!     protocol wakes it.
//!
//! The caller's rules (the device protocols' job, not the bus's): when a
//! device may start a transaction (BUSY gates) and what the bytes mean.
//!
//! Caller invariants:
//!   - [`Bus::bring_up`] runs once the SD card has had power for ≥ 1 ms
//!     (it's on the always-on rail, so any time after boot).
//!   - One `Bus` per `Wire`; nothing else drives the wire's outputs.

use super::wire::{Hz, Input, Output, Wire, WireError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Device {
    Epaper,
    SdCard,
    Lora,
}

impl Device {
    pub const ALL: [Device; 3] = [Device::Epaper, Device::SdCard, Device::Lora];

    /// Fastest clock the device accepts (`docs/spi-research.md`):
    /// e-paper TSCYCW ≥ 170 ns (§2.1), SD 25 MHz once initialised (§3.1;
    /// ≤ 400 kHz until then is the SD protocol's job), SX1262 16 MHz (§4).
    pub const fn max_clock(self) -> Hz {
        match self {
            Device::Epaper => Hz(5_880_000),
            Device::SdCard => Hz(25_000_000),
            Device::Lora => Hz(16_000_000),
        }
    }

    pub const fn cs(self) -> Output {
        match self {
            Device::Epaper => Output::EpdCs,
            Device::SdCard => Output::SdCs,
            Device::Lora => Output::LoraCs,
        }
    }
}

/// One part of a transaction. Read parts send `0xFF` (MOSI high).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Part {
    /// E-paper only: sent with DC low.
    Command(Vec<u8>),
    /// Sent (DC high for the e-paper); MISO ignored.
    Write(Vec<u8>),
    /// Receive this many bytes.
    Read(usize),
    /// Send these bytes and keep what comes back.
    Exchange(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub device: Device,
    pub clock: Hz,
    /// May be empty: a chip select pulse (wakes a sleeping SX1262).
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// [`Bus::bring_up`] hasn't run.
    NotBroughtUp,
    /// `clock` is above the device's limit; nothing was sent.
    ClockTooFast,
    /// A `Command` part for a device other than the e-paper; nothing sent.
    InvalidPart,
    /// The wire failed; an unknown prefix was sent. Chip selects are high
    /// again and the release byte was attempted.
    Wire(WireError),
}

/// Clock for bring-up: SD cards accept 100–400 kHz before initialisation.
pub const BRING_UP_CLOCK: Hz = Hz(400_000);

/// CMD0 (GO_IDLE_STATE) with its fixed CRC; the card checks the CRC.
const CMD0: [u8; 6] = [0x40, 0x00, 0x00, 0x00, 0x00, 0x95];

/// ≥ 74 idle clocks for the SD card, in whole bytes.
const IDLE_BYTES: usize = 10;

/// Bytes to read after CMD0: the response comes within 8 (NCR).
const CMD0_RESPONSE_BYTES: usize = 8;

/// SX1262 SetSleep, cold start (configuration not kept), no RTC wake-up.
pub const LORA_SLEEP: [u8; 2] = [0x84, 0x00];

/// After power-on, before LoRa BUSY means anything: the pin reads low
/// while the chip is still unpowered, then high while it boots.
const LORA_BOOT_US: u32 = 5_000;

/// Longest wait, after that, for LoRa BUSY low (cold start ~3.5 ms).
const LORA_READY_US: u32 = 20_000;
const LORA_POLL_US: u32 = 100;

#[derive(Debug, Default)]
pub struct Bus {
    up: bool,
    /// The last release byte failed to go out: send it before the next
    /// chip select goes low (B2).
    release_owed: bool,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Put every pin in its idle state, every SD card into SPI mode and
    /// the LoRa module to sleep (B4, B5). Running it again is harmless.
    pub fn bring_up<W: Wire>(&mut self, w: &mut W) -> Result<(), WireError> {
        w.set(Output::EpdCs, true);
        w.set(Output::SdCs, true);
        w.set(Output::EpdDc, true);
        w.set(Output::LoraCs, true);
        w.set(Output::LoraRst, true);
        w.set(Output::LoraEn, true);
        w.pause_us(LORA_BOOT_US);
        w.transfer(BRING_UP_CLOCK, &[0xFF; IDLE_BYTES], None)?;
        self.release_owed = false;
        // The answer doesn't matter: with no card, MISO reads 0xFF.
        let cmd0 = Transaction {
            device: Device::SdCard,
            clock: BRING_UP_CLOCK,
            parts: vec![Part::Write(CMD0.to_vec()), Part::Read(CMD0_RESPONSE_BYTES)],
        };
        self.up = true;
        self.run_or_down(w, &cmd0)?;
        // Already asleep (bring-up running again) also leaves BUSY high.
        if self.wait_lora_ready(w) {
            let sleep = Transaction { device: Device::Lora, clock: BRING_UP_CLOCK, parts: vec![Part::Write(LORA_SLEEP.to_vec())] };
            self.run_or_down(w, &sleep)?;
        } else {
            log::warn!("spi: LoRa BUSY still high after power-on; not put to sleep");
        }
        Ok(())
    }

    fn run_or_down<W: Wire>(&mut self, w: &mut W, t: &Transaction) -> Result<(), WireError> {
        match self.run(w, t) {
            Ok(_) => Ok(()),
            Err(BusError::Wire(e)) => {
                self.up = false;
                Err(e)
            }
            Err(e) => unreachable!("bring-up transaction is valid: {e:?}"),
        }
    }

    fn wait_lora_ready<W: Wire>(&self, w: &mut W) -> bool {
        for _ in 0..LORA_READY_US / LORA_POLL_US {
            if !w.get(Input::LoraBusy) {
                return true;
            }
            w.pause_us(LORA_POLL_US);
        }
        false
    }

    pub fn is_up(&self) -> bool {
        self.up
    }

    /// Run one transaction; returns the bytes of its `Read` and `Exchange`
    /// parts, in order.
    pub fn run<W: Wire>(&mut self, w: &mut W, t: &Transaction) -> Result<Vec<u8>, BusError> {
        if !self.up {
            return Err(BusError::NotBroughtUp);
        }
        if t.clock > t.device.max_clock() {
            return Err(BusError::ClockTooFast);
        }
        if t.device != Device::Epaper && t.parts.iter().any(|p| matches!(p, Part::Command(_))) {
            return Err(BusError::InvalidPart);
        }
        if self.release_owed {
            w.transfer(BRING_UP_CLOCK, &[0xFF], None).map_err(BusError::Wire)?;
            self.release_owed = false;
        }
        let cs = t.device.cs();
        w.set(cs, false);
        let result = run_parts(w, t);
        // Always deselect and release, even after a failure (B1, B2).
        w.set(cs, true);
        if t.device == Device::Epaper {
            w.set(Output::EpdDc, true);
        }
        let release = w.transfer(t.clock, &[0xFF], None);
        self.release_owed = release.is_err();
        let input = result.map_err(BusError::Wire)?;
        release.map_err(BusError::Wire)?;
        Ok(input)
    }

    pub fn read<W: Wire>(&self, w: &mut W, pin: Input) -> bool {
        w.get(pin)
    }
}

/// The parts of `t`, with its chip select already low.
fn run_parts<W: Wire>(w: &mut W, t: &Transaction) -> Result<Vec<u8>, WireError> {
    let epaper = t.device == Device::Epaper;
    let mut input = Vec::new();
    for part in &t.parts {
        if epaper {
            w.set(Output::EpdDc, !matches!(part, Part::Command(_)));
        }
        match part {
            Part::Command(bytes) | Part::Write(bytes) => w.transfer(t.clock, bytes, None)?,
            Part::Read(n) => {
                let start = input.len();
                input.resize(start + n, 0);
                w.transfer(t.clock, &vec![0xFF; *n], Some(&mut input[start..]))?;
            }
            Part::Exchange(bytes) => {
                let start = input.len();
                input.resize(start + bytes.len(), 0);
                w.transfer(t.clock, bytes, Some(&mut input[start..]))?;
            }
        }
    }
    Ok(input)
}

#[cfg(test)]
mod tests;
