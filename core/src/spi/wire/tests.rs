//! `MockWire`: a wire-level model of the T-Deck-Pro's SPI bus.
//!
//! It tracks every pin, routes each clocked byte to the device whose chip
//! select is low, and **panics on any violation of the bus rules B1–B5**
//! (see `spi::bus`), so every test that drives it checks them. The limits
//! here are written down independently of `bus.rs`: this is the oracle.
//!
//! Devices answer through a [`SimDevice`] when one is attached, else from
//! a queue of scripted replies, else `0xFF` (MISO has a pull-up, and an
//! unselected or absent device leaves it floating high).
//!
//! What it models:
//!   - B1 at most one active device selected (LoRa active = powered and
//!     out of reset), and only with every chip select driven.
//!   - B2 after any device is deselected, one byte must be clocked with
//!     every chip select high before a chip select goes low again.
//!   - B3 clock limits per device (and ≤ 400 kHz for the SD card until it
//!     is in SPI mode).
//!   - B4 the SD card starts in SD mode; it enters SPI mode only after
//!     ≥ 74 idle clocks at ≤ 400 kHz (CS and MOSI high) followed by a
//!     selection that starts with CMD0. Selecting any other device first
//!     panics, as does selecting the card for anything else first.
//!   - B5 no clocks while the LoRa supply is off (an unpowered SX1262
//!     blocks SCK/MOSI; measured on hardware).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use super::{Clock, Hz, Input, Output, Wire, WireError};
use crate::spi::bus::Device;
use crate::spi::Millis;

/// A simulated chip on the bus. `now` is the mock's clock
/// ([`MockWire::set_now`]).
pub(crate) trait SimDevice {
    fn select(&mut self, _now: Millis) {}
    fn deselect(&mut self, _now: Millis) {}
    /// One byte clocked while selected; `dc` is the e-paper DC level.
    /// Returns what the chip puts on MISO.
    fn clock(&mut self, mosi: u8, dc: bool, now: Millis) -> u8;
    /// The level this chip drives on `pin`, if it drives it.
    fn pin(&mut self, _pin: Input, _now: Millis) -> Option<bool> {
        None
    }
}

/// A simulator the test keeps a handle to while the mock drives it.
pub(crate) struct Shared<T>(pub Rc<RefCell<T>>);

impl<T: SimDevice> SimDevice for Shared<T> {
    fn select(&mut self, now: Millis) {
        self.0.borrow_mut().select(now)
    }
    fn deselect(&mut self, now: Millis) {
        self.0.borrow_mut().deselect(now)
    }
    fn clock(&mut self, mosi: u8, dc: bool, now: Millis) -> u8 {
        self.0.borrow_mut().clock(mosi, dc, now)
    }
    fn pin(&mut self, pin: Input, now: Millis) -> Option<bool> {
        self.0.borrow_mut().pin(pin, now)
    }
}

/// One byte a device received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Byte {
    pub value: u8,
    pub dc: bool,
    pub clock: Hz,
}

/// Everything a device received during one selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Frame {
    pub device: Device,
    pub bytes: Vec<Byte>,
    /// Time ([`MockWire::time_ns`]) at selection.
    pub start_ns: u64,
}

impl Frame {
    pub fn values(&self) -> Vec<u8> {
        self.bytes.iter().map(|b| b.value).collect()
    }
}

/// What happened on the wire, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    Set(Output, bool),
    /// Bytes clocked, and which device was selected (`None`: idle clocks).
    Transfer { selected: Option<Device>, len: usize },
    Pause(u32),
}

/// Fastest clocks, from `docs/spi-research.md` §2.1, §3.1, §4.
fn limit(device: Device, sd_spi_mode: bool) -> Hz {
    match device {
        Device::Epaper => Hz(5_880_000),
        Device::SdCard if sd_spi_mode => Hz(25_000_000),
        Device::SdCard => Hz(400_000),
        Device::Lora => Hz(16_000_000),
    }
}

const CMD0: [u8; 6] = [0x40, 0x00, 0x00, 0x00, 0x00, 0x95];
const SD_IDLE_CLOCKS: u32 = 74;

#[derive(Default)]
pub(crate) struct MockWire {
    /// Absent = never driven (floating).
    outputs: HashMap<Output, bool>,
    inputs: HashMap<Input, bool>,
    sims: HashMap<Device, Box<dyn SimDevice>>,
    replies: HashMap<Device, VecDeque<u8>>,
    selected: Option<Device>,
    release_pending: bool,
    idle_clocks: u32,
    sd_spi_mode: bool,
    current: Option<Frame>,
    frames: Vec<Frame>,
    events: Vec<Event>,
    /// Transfers left before an injected failure.
    fail_in: Option<usize>,
    /// Simulated time: advanced by transfers and pauses as on real
    /// hardware, and by the test with [`MockWire::set_now`].
    time_ns: u64,
}

impl MockWire {
    pub fn new() -> Self {
        let mut w = Self::default();
        w.inputs.insert(Input::EpdBusy, true);
        w.inputs.insert(Input::LoraBusy, false);
        w.inputs.insert(Input::LoraDio1, false);
        w
    }

    pub fn attach(&mut self, device: Device, sim: Box<dyn SimDevice>) {
        self.sims.insert(device, sim);
    }

    /// Attach `sim` and keep a handle to it.
    pub fn attach_shared<T: SimDevice + 'static>(&mut self, device: Device, sim: T) -> Rc<RefCell<T>> {
        let rc = Rc::new(RefCell::new(sim));
        self.attach(device, Box::new(Shared(rc.clone())));
        rc
    }

    /// Move simulated time forward to `now` (never back).
    pub fn set_now(&mut self, now: Millis) {
        self.time_ns = self.time_ns.max(now * 1_000_000);
    }

    /// Bytes `device` puts on MISO next, when no simulator is attached.
    pub fn reply(&mut self, device: Device, bytes: &[u8]) {
        self.replies.entry(device).or_default().extend(bytes);
    }

    pub fn set_input(&mut self, pin: Input, high: bool) {
        self.inputs.insert(pin, high);
    }

    /// Fail the transfer after the next `n` successful ones (once).
    pub fn fail_transfer_after(&mut self, n: usize) {
        self.fail_in = Some(n);
    }

    pub fn output(&self, pin: Output) -> Option<bool> {
        self.outputs.get(&pin).copied()
    }

    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn sd_spi_mode(&self) -> bool {
        self.sd_spi_mode
    }

    pub fn time_ns(&self) -> u64 {
        self.time_ns
    }

    /// Nothing selected and no release byte owed: the bus is idle.
    pub fn assert_idle(&self) {
        assert_eq!(self.selected, None, "a device is still selected");
        assert!(!self.release_pending, "release byte still owed");
    }

    pub fn assert_no_selection(&self) {
        assert_eq!(self.selected, None, "a device is still selected");
    }

    pub fn release_pending(&self) -> bool {
        self.release_pending
    }

    fn active(&self, d: Device) -> bool {
        match d {
            Device::Epaper | Device::SdCard => true,
            Device::Lora => self.output(Output::LoraEn) == Some(true) && self.output(Output::LoraRst) == Some(true),
        }
    }

    fn cs_low(&self, d: Device) -> bool {
        self.output(d.cs()) == Some(false)
    }

    fn check_b5(&self) {
        assert_eq!(self.output(Output::LoraEn), Some(true), "B5: clocks while the LoRa supply is off");
    }

    /// Recompute which device is selected after a pin change, handling the
    /// edges.
    fn update_selection(&mut self) {
        let selected: Vec<Device> = Device::ALL.into_iter().filter(|&d| self.active(d) && self.cs_low(d)).collect();
        assert!(selected.len() <= 1, "B1: {selected:?} selected at once");
        let now = selected.first().copied();
        if now == self.selected {
            return;
        }
        if let Some(old) = self.selected.take() {
            self.deselect(old);
        }
        if let Some(new) = now {
            self.select(new);
        }
    }

    fn select(&mut self, d: Device) {
        assert!(!self.release_pending, "B2: {d:?} selected before the release byte");
        for other in [Device::Epaper, Device::SdCard] {
            assert!(self.output(other.cs()).is_some(), "B1: {d:?} selected while {other:?} CS is undriven");
        }
        if !self.sd_spi_mode {
            assert_eq!(d, Device::SdCard, "B4: {d:?} selected before the SD card is in SPI mode");
            assert!(
                self.idle_clocks >= SD_IDLE_CLOCKS,
                "B4: SD card selected after {} idle clocks (needs {SD_IDLE_CLOCKS})",
                self.idle_clocks
            );
        }
        self.selected = Some(d);
        self.current = Some(Frame { device: d, bytes: Vec::new(), start_ns: self.time_ns });
        let now = self.now();
        if let Some(sim) = self.sims.get_mut(&d) {
            sim.select(now);
        }
    }

    fn deselect(&mut self, d: Device) {
        let frame = self.current.take().expect("selected device has a frame");
        if d == Device::SdCard && !self.sd_spi_mode {
            assert!(frame.values().starts_with(&CMD0), "B4: first SD selection didn't start with CMD0");
            self.sd_spi_mode = true;
        }
        self.frames.push(frame);
        self.release_pending = true;
        let now = self.now();
        if let Some(sim) = self.sims.get_mut(&d) {
            sim.deselect(now);
        }
    }

    fn miso(&mut self, d: Device, mosi: u8, dc: bool) -> u8 {
        let now = self.now();
        if let Some(sim) = self.sims.get_mut(&d) {
            return sim.clock(mosi, dc, now);
        }
        self.replies.get_mut(&d).and_then(|q| q.pop_front()).unwrap_or(0xFF)
    }
}

impl Clock for MockWire {
    fn now(&self) -> Millis {
        self.time_ns / 1_000_000
    }
}

impl Wire for MockWire {
    fn set(&mut self, pin: Output, high: bool) {
        self.events.push(Event::Set(pin, high));
        self.outputs.insert(pin, high);
        self.update_selection();
    }

    fn get(&mut self, pin: Input) -> bool {
        let now = self.now();
        for sim in self.sims.values_mut() {
            if let Some(level) = sim.pin(pin, now) {
                return level;
            }
        }
        self.inputs[&pin]
    }

    fn transfer(&mut self, clock: Hz, out: &[u8], mut input: Option<&mut [u8]>) -> Result<(), WireError> {
        self.check_b5();
        if let Some(n) = self.fail_in {
            if n == 0 {
                self.fail_in = None;
                return Err(WireError("injected".into()));
            }
            self.fail_in = Some(n - 1);
        }
        if let Some(buf) = &input {
            assert_eq!(buf.len(), out.len(), "input buffer length");
        }
        self.events.push(Event::Transfer { selected: self.selected, len: out.len() });
        self.time_ns += out.len() as u64 * 8 * 1_000_000_000 / clock.0 as u64;
        match self.selected {
            None => {
                if !out.is_empty() {
                    self.release_pending = false;
                }
                if !self.sd_spi_mode && clock <= Hz(400_000) && out.iter().all(|&b| b == 0xFF) {
                    self.idle_clocks += 8 * out.len() as u32;
                }
                if let Some(buf) = input {
                    buf.fill(0xFF);
                }
            }
            Some(d) => {
                let max = limit(d, self.sd_spi_mode);
                assert!(clock <= max, "B3: {d:?} clocked at {clock:?}, limit {max:?}");
                let dc = self.output(Output::EpdDc) == Some(true);
                for (i, &mosi) in out.iter().enumerate() {
                    let miso = self.miso(d, mosi, dc);
                    if let Some(buf) = input.as_deref_mut() {
                        buf[i] = miso;
                    }
                    self.current.as_mut().expect("frame").bytes.push(Byte { value: mosi, dc, clock });
                }
            }
        }
        Ok(())
    }

    fn pause_us(&mut self, us: u32) {
        self.events.push(Event::Pause(us));
        self.time_ns += us as u64 * 1_000;
    }
}

/// The mock's own rule checks, driven by hand (independently of `Bus`).
mod checks {
    use super::*;

    const SLOW: Hz = Hz(400_000);

    /// Chip selects high, LoRa powered and deselected: ready for B4.
    fn fresh() -> MockWire {
        let mut w = MockWire::new();
        w.set(Output::EpdCs, true);
        w.set(Output::SdCs, true);
        w.set(Output::LoraCs, true);
        w.set(Output::LoraRst, true);
        w.set(Output::LoraEn, true);
        w
    }

    /// A correct bring-up, by hand.
    fn up() -> MockWire {
        let mut w = fresh();
        w.transfer(SLOW, &[0xFF; 10], None).unwrap();
        w.set(Output::SdCs, false);
        w.transfer(SLOW, &CMD0, None).unwrap();
        w.set(Output::SdCs, true);
        w.transfer(SLOW, &[0xFF], None).unwrap();
        w
    }

    #[test]
    fn hand_bring_up_is_accepted() {
        let w = up();
        assert!(w.sd_spi_mode());
        w.assert_idle();
        assert_eq!(w.frames().len(), 1);
        assert_eq!(w.frames()[0].values(), CMD0);
    }

    #[test]
    #[should_panic(expected = "B1")]
    fn two_selected_at_once() {
        let mut w = up();
        w.set(Output::EpdCs, false);
        w.set(Output::SdCs, false);
    }

    #[test]
    #[should_panic(expected = "B2")]
    fn select_without_release_byte() {
        let mut w = up();
        w.set(Output::EpdCs, false);
        w.set(Output::EpdCs, true);
        w.set(Output::EpdCs, false);
    }

    #[test]
    #[should_panic(expected = "B3")]
    fn epaper_too_fast() {
        let mut w = up();
        w.set(Output::EpdCs, false);
        w.transfer(Hz(10_000_000), &[0], None).unwrap();
    }

    #[test]
    #[should_panic(expected = "B3")]
    fn sd_fast_before_spi_mode() {
        let mut w = fresh();
        w.transfer(SLOW, &[0xFF; 10], None).unwrap();
        w.set(Output::SdCs, false);
        w.transfer(Hz(1_000_000), &CMD0, None).unwrap();
    }

    #[test]
    #[should_panic(expected = "B4")]
    fn epaper_before_sd_cmd0() {
        let mut w = fresh();
        w.transfer(SLOW, &[0xFF; 10], None).unwrap();
        w.set(Output::EpdCs, false);
    }

    #[test]
    #[should_panic(expected = "B4")]
    fn sd_selected_too_early() {
        let mut w = fresh();
        w.transfer(SLOW, &[0xFF; 9], None).unwrap(); // 72 clocks
        w.set(Output::SdCs, false);
    }

    #[test]
    #[should_panic(expected = "B4")]
    fn sd_first_selection_not_cmd0() {
        let mut w = fresh();
        w.transfer(SLOW, &[0xFF; 10], None).unwrap();
        w.set(Output::SdCs, false);
        w.transfer(SLOW, &[0x51, 0, 0, 0, 0, 0xFF], None).unwrap();
        w.set(Output::SdCs, true);
    }

    #[test]
    #[should_panic(expected = "B5")]
    fn clocks_before_lora_powered() {
        let mut w = MockWire::new();
        w.set(Output::EpdCs, true);
        w.set(Output::SdCs, true);
        w.transfer(SLOW, &[0xFF; 10], None).unwrap();
    }

    #[test]
    #[should_panic(expected = "B5")]
    fn clocks_after_lora_supply_off() {
        let mut w = up();
        w.set(Output::LoraEn, false);
        w.transfer(SLOW, &[0xFF], None).unwrap();
    }

    #[test]
    #[should_panic(expected = "B1")]
    fn selected_with_undriven_chip_select() {
        let mut w = MockWire::new();
        w.set(Output::SdCs, false);
    }

    #[test]
    fn lora_in_reset_ignores_its_chip_select() {
        let mut w = up();
        w.set(Output::LoraRst, false);
        w.set(Output::LoraCs, false);
        // CS low, chip in reset: not selected, so the e-paper may be.
        w.set(Output::EpdCs, false);
        w.set(Output::EpdCs, true);
        w.transfer(SLOW, &[0xFF], None).unwrap();
        w.assert_idle();
    }

    #[test]
    fn inputs_default_to_idle_levels_and_can_be_set() {
        let mut w = up();
        assert!(w.get(Input::EpdBusy), "e-paper not busy");
        assert!(!w.get(Input::LoraBusy));
        w.set_input(Input::LoraBusy, true);
        assert!(w.get(Input::LoraBusy));
    }

    #[test]
    fn simulated_time_follows_transfers_pauses_and_set_now() {
        let mut w = up();
        let before = w.time_ns();
        w.transfer(Hz(1_000_000), &[0xFF; 125], None).unwrap(); // 1000 bits
        w.pause_us(3);
        assert_eq!(w.time_ns() - before, 1_000_000 + 3_000);
        w.set_now(50);
        assert_eq!(w.now(), 50);
        w.set_now(10); // never back
        assert_eq!(w.now(), 50);
    }

    #[test]
    fn unselected_and_absent_devices_read_ff() {
        let mut w = up();
        let mut buf = [0u8; 2];
        w.transfer(SLOW, &[0, 0], Some(&mut buf)).unwrap();
        assert_eq!(buf, [0xFF, 0xFF]);
        w.set(Output::SdCs, false);
        w.transfer(SLOW, &[0, 0], Some(&mut buf)).unwrap();
        assert_eq!(buf, [0xFF, 0xFF]);
    }

    #[test]
    fn scripted_replies_and_dc_are_recorded() {
        let mut w = up();
        w.reply(Device::Epaper, &[7]);
        w.set(Output::EpdDc, false);
        w.set(Output::EpdCs, false);
        let mut buf = [0u8; 1];
        w.transfer(Hz(5_000_000), &[0x12], Some(&mut buf)).unwrap();
        w.set(Output::EpdCs, true);
        assert_eq!(buf, [7]);
        let f = w.frames().last().unwrap();
        assert_eq!(f.device, Device::Epaper);
        assert_eq!(f.bytes, vec![Byte { value: 0x12, dc: false, clock: Hz(5_000_000) }]);
        assert!(f.start_ns > 0);
    }
}
