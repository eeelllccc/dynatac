//! `SimEpaper`: the UC8253 as our protocol uses it, for `MockWire`.
//!
//! Models (from `docs/spi-research.md` §2): the command set we send and
//! its parameter counts; the two image buffers (0x10 previous, 0x13
//! current) written through the partial window or whole; what the screen
//! shows after each refresh; BUSY low during power on/off, soft reset and
//! refresh, with an optional lag before it falls; the soft reset switching
//! the drive voltages off.
//!
//! Panics on a protocol violation: any byte while the panel is busy (only
//! reads are allowed then, and we don't send reads), an unmodelled
//! command, data with no command, a wrong parameter count, a refresh with
//! the drive voltages off, image data beyond the window.
//!
//! A partial refresh only drives pixels correctly where the previous
//! buffer matches the screen; anywhere else the screen is marked invalid
//! (ghosting), which tests check via [`SimEpaper::screen_valid`].
//!
//! Faults: `tolerate_cut_off` accepts a command whose parameters never
//! came (a new command byte starts a new command, as on the real panel).
//! `stuck` holds BUSY at a level. Stuck high, bytes sent while the
//! panel is really busy are ignored (as the real panel ignores them), not
//! treated as violations: the protocol had no way to know.

use crate::framebuffer::{BUF_LEN, BYTES_PER_ROW};
use crate::spi::wire::tests::SimDevice;
use crate::spi::wire::Input;
use crate::spi::Millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refresh {
    Partial,
    Full,
}

pub(crate) struct SimEpaper {
    pub pon_ms: Millis,
    pub pof_ms: Millis,
    pub reset_ms: Millis,
    pub partial_ms: Millis,
    pub full_ms: Millis,
    /// How long after a busy command BUSY actually falls.
    pub busy_lag_ms: Millis,
    /// BUSY stuck at this level (a fault).
    pub stuck: Option<bool>,
    /// Accept a command cut off before its parameters (what a wire failure
    /// mid-command leaves). Otherwise that panics.
    pub tolerate_cut_off: bool,
    cmd: Option<u8>,
    params: Vec<u8>,
    powered: bool,
    /// Internally busy from `.0`, BUSY pin low from `.0 + lag`, until `.1`.
    busy: Option<(Millis, Millis)>,
    partial_mode: bool,
    /// x start/end byte (inclusive), y start/end row (inclusive).
    window: (usize, usize, usize, usize),
    pos: usize,
    old: Vec<u8>,
    new: Vec<u8>,
    screen: Vec<u8>,
    screen_valid: bool,
    /// Completed commands with their parameters (image data not included).
    pub log: Vec<(u8, Vec<u8>)>,
    pub refreshes: Vec<Refresh>,
}

impl SimEpaper {
    /// Typical timings (GxEPD2's constants); contents unknown.
    pub fn new() -> Self {
        SimEpaper {
            pon_ms: 50,
            pof_ms: 50,
            reset_ms: 1,
            partial_ms: 650,
            full_ms: 1015,
            busy_lag_ms: 0,
            stuck: None,
            tolerate_cut_off: false,
            cmd: None,
            params: Vec::new(),
            powered: false,
            busy: None,
            partial_mode: false,
            window: (0, 0, 0, 0),
            pos: 0,
            old: vec![0; BUF_LEN],
            new: vec![0; BUF_LEN],
            screen: vec![0; BUF_LEN],
            screen_valid: false,
            log: Vec::new(),
            refreshes: Vec::new(),
        }
    }

    pub fn screen(&self) -> &[u8] {
        &self.screen
    }

    pub fn old(&self) -> &[u8] {
        &self.old
    }

    /// Every pixel on screen was driven correctly.
    pub fn screen_valid(&self) -> bool {
        self.screen_valid
    }

    pub fn powered(&self) -> bool {
        self.powered
    }

    fn really_busy(&self, now: Millis) -> bool {
        self.busy.is_some_and(|(from, until)| from <= now && now < until)
    }

    fn pin_high(&self, now: Millis) -> bool {
        if let Some(level) = self.stuck {
            return level;
        }
        !self.busy.is_some_and(|(from, until)| from + self.busy_lag_ms <= now && now < until)
    }

    fn start_busy(&mut self, now: Millis, ms: Millis) {
        self.busy = Some((now, now + self.busy_lag_ms + ms));
    }

    fn param_count(c: u8) -> Option<usize> {
        match c {
            0x00 => Some(2),
            0x90 => Some(7),
            0xE0 | 0xE5 | 0x50 => Some(1),
            _ => None,
        }
    }

    fn command(&mut self, c: u8, now: Millis) {
        if let Some(prev) = self.cmd {
            if let Some(n) = Self::param_count(prev) {
                if self.params.len() != n {
                    assert!(self.tolerate_cut_off, "command 0x{prev:02X} got {} params", self.params.len());
                }
            }
        }
        self.cmd = None;
        self.params.clear();
        match c {
            0x00 | 0x90 | 0xE0 | 0xE5 | 0x50 => self.cmd = Some(c),
            0x10 | 0x13 => {
                self.cmd = Some(c);
                self.pos = 0;
                self.log.push((c, vec![]));
            }
            0x04 => {
                self.powered = true;
                self.start_busy(now, self.pon_ms);
                self.log.push((c, vec![]));
            }
            0x02 => {
                self.powered = false;
                self.start_busy(now, self.pof_ms);
                self.log.push((c, vec![]));
            }
            0x12 => {
                assert!(self.powered, "refresh with the drive voltages off");
                self.refresh(now);
                self.log.push((c, vec![]));
            }
            0x91 => {
                self.partial_mode = true;
                self.log.push((c, vec![]));
            }
            0x92 => {
                self.partial_mode = false;
                self.log.push((c, vec![]));
            }
            _ => panic!("unmodelled command 0x{c:02X}"),
        }
    }

    fn data(&mut self, b: u8, now: Millis) {
        match self.cmd {
            Some(reg @ (0x10 | 0x13)) => {
                let i = self.data_index(self.pos);
                self.pos += 1;
                if reg == 0x10 {
                    self.old[i] = b;
                } else {
                    self.new[i] = b;
                }
            }
            Some(c) => {
                self.params.push(b);
                let n = Self::param_count(c).expect("parameter command");
                if self.params.len() == n {
                    self.execute(c, now);
                    self.log.push((c, std::mem::take(&mut self.params)));
                    self.cmd = None;
                }
            }
            None => panic!("data byte 0x{b:02X} with no command"),
        }
    }

    /// Where the `pos`th byte of image data goes.
    fn data_index(&self, pos: usize) -> usize {
        if !self.partial_mode {
            assert!(pos < BUF_LEN, "image data beyond the screen");
            return pos;
        }
        let (x0, x1, y0, y1) = self.window;
        let wb = x1 - x0 + 1;
        let (row, col) = (pos / wb, pos % wb);
        assert!(y0 + row <= y1, "image data beyond the window");
        (y0 + row) * BYTES_PER_ROW + x0 + col
    }

    fn execute(&mut self, c: u8, now: Millis) {
        let p = &self.params;
        match c {
            0x00 if p[0] & 0x01 == 0 => {
                // Soft reset: registers to defaults, booster off.
                self.powered = false;
                self.partial_mode = false;
                self.start_busy(now, self.reset_ms);
            }
            0x90 => {
                let y0 = (p[2] as usize) << 8 | p[3] as usize;
                let y1 = (p[4] as usize) << 8 | p[5] as usize;
                self.window = (p[0] as usize / 8, p[1] as usize / 8, y0, y1);
            }
            _ => {}
        }
    }

    fn refresh(&mut self, now: Millis) {
        if self.partial_mode {
            let (x0, x1, y0, y1) = self.window;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let i = y * BYTES_PER_ROW + x;
                    if self.old[i] != self.screen[i] {
                        self.screen_valid = false;
                    }
                    self.screen[i] = self.new[i];
                }
            }
            self.refreshes.push(Refresh::Partial);
            self.start_busy(now, self.partial_ms);
        } else {
            self.screen.copy_from_slice(&self.new);
            self.screen_valid = true;
            self.refreshes.push(Refresh::Full);
            self.start_busy(now, self.full_ms);
        }
    }
}

impl SimDevice for SimEpaper {
    fn clock(&mut self, mosi: u8, dc: bool, now: Millis) -> u8 {
        if self.really_busy(now) {
            if self.stuck == Some(true) {
                return 0xFF; // ignored, as the real panel would
            }
            panic!("byte 0x{mosi:02X} (dc {dc}) sent while the panel is busy");
        }
        if dc {
            self.data(mosi, now);
        } else {
            self.command(mosi, now);
        }
        0xFF // no MISO: the panel's output is on SDA, unused
    }

    fn pin(&mut self, pin: Input, now: Millis) -> Option<bool> {
        (pin == Input::EpdBusy).then(|| self.pin_high(now))
    }
}
