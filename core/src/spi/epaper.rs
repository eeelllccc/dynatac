//! The GDEQ031T10 e-paper (UC8253 controller) as a bus protocol.
//!
//! Pure state machine (see [`protocol`](super::protocol)): requests in,
//! transactions and waits out, one [`Done`] per request, in order. Each
//! request becomes a plain list of steps up front ([`Epaper::plan`]), so
//! the exact command sequence for every operation can be read and tested.
//!
//! Sequences are the ones our previous driver ran on this hardware, which
//! match GxEPD2's (`docs/spi-research.md` §2). The controller keeps two
//! image buffers: 0x10 "previous" and 0x13 "current". A partial refresh
//! drives each pixel by comparing them, so after every refresh the
//! previous buffer must be rewritten to equal what is now on screen.
//!
//! Callee invariants:
//!   - Every request gets exactly one [`Done`], in request order.
//!   - No command is sent while BUSY is low (the panel only takes reads
//!     then): every job starts by waiting for BUSY high (after a settle:
//!     a failed job may just have sent a busy command), and each command
//!     that starts internal work is followed by a wait for BUSY high.
//!   - BUSY is not sampled sooner than [`SETTLE_MS`] after such a command
//!     (the panel takes up to ~200 µs to pull it low).
//!   - Every wait has a deadline. A timeout or bus failure ends the request
//!     with an error and marks the panel's contents unknown.
//!   - While the contents are unknown (at start, and after any failure), a
//!     partial draw is refused with [`Fault::ContentsUnknown`] without
//!     touching the panel; a full draw makes them known again.
//!   - Image data goes in transactions of at most [`CHUNK`] bytes.
//!
//! Caller invariants: the protocol contract in [`protocol`](super::protocol).

use std::collections::VecDeque;

use super::bus::{BusError, Device, Part, Transaction};
use super::protocol::{Next, Outcome};
use super::wire::{Hz, Input};
use super::Millis;
use crate::framebuffer::{Flush, BUF_LEN, HEIGHT, WIDTH};

/// SPI clock: within the controller's 5.88 MHz write limit, and exactly
/// reachable from the 80 MHz APB clock (÷16).
pub const CLOCK: Hz = Hz(5_000_000);

/// Largest transaction: the ESP32-S3 DMA default `max_transfer_sz`.
/// ≈ 6.5 ms at [`CLOCK`], which bounds how long others wait for the bus.
pub const CHUNK: usize = 4092;

/// Wait after a busy command before BUSY means anything.
pub const SETTLE_MS: Millis = 1;

/// BUSY deadlines, generous multiples of the typical times (power on/off
/// ~50 ms, partial ~0.65 s, full ~1.0 s fast / ~3.1 s compensated).
pub const POWER_TIMEOUT_MS: Millis = 1_000;
pub const RESET_TIMEOUT_MS: Millis = 100;
pub const PARTIAL_TIMEOUT_MS: Millis = 5_000;
pub const FULL_TIMEOUT_MS: Millis = 10_000;
/// Longest wait for BUSY high before starting a job.
pub const READY_TIMEOUT_MS: Millis = FULL_TIMEOUT_MS;

/// A byte-aligned area of the screen, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Window {
    /// `x` and `w` multiples of 8, non-empty, on screen.
    pub fn is_valid(&self) -> bool {
        self.x % 8 == 0
            && self.w % 8 == 0
            && self.w > 0
            && self.h > 0
            && self.x as u32 + self.w as u32 <= WIDTH as u32
            && self.y as u32 + self.h as u32 <= HEIGHT as u32
    }

    /// Bytes of image data for this window (1 bit per pixel, row by row).
    pub fn data_len(&self) -> usize {
        (self.w as usize / 8) * self.h as usize
    }

    /// Parameters of the partial-window command (0x90): x start and end
    /// (inclusive, byte-aligned), y start and end (16-bit), scan mode.
    fn params(&self) -> Vec<u8> {
        let x_end = (self.x + self.w - 1) | 0x0007;
        let y_end = self.y + self.h - 1;
        vec![self.x as u8, x_end as u8, (self.y >> 8) as u8, self.y as u8, (y_end >> 8) as u8, y_end as u8, 0x01]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Show `data` (1 = white, row by row, `window.data_len()` bytes) in
    /// `window` with a partial refresh. Needs the contents to be known.
    Partial { window: Window, data: Vec<u8> },
    /// Show a whole screen (`BUF_LEN` bytes) with a full refresh.
    Full { data: Vec<u8> },
    /// Switch the panel's drive voltages off (the image stays).
    PowerOff,
}

/// What the framebuffer wants sent becomes a draw request.
impl From<Flush> for Request {
    fn from(flush: Flush) -> Self {
        match flush {
            Flush::Full(data) => Request::Full { data },
            Flush::Partial(r) => Request::Partial { window: Window { x: r.x, y: r.y, w: r.w, h: r.h }, data: r.data },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    /// Bad window or data length; nothing was sent.
    Invalid,
    /// A partial draw while the panel's contents are unknown; nothing was
    /// sent. Send a full draw.
    ContentsUnknown,
    /// BUSY didn't go high in time.
    Timeout,
    Bus(BusError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Done {
    Drawn(Result<(), Fault>),
    PoweredOff(Result<(), Fault>),
}

/// Controller state the protocol tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flag {
    /// Panel setting sent since the last refresh / reset.
    Init,
    /// Drive voltages on (PON sent since the last POF / soft reset).
    Power,
    /// The previous buffer and the screen show what we last drew.
    Known,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Tx(Vec<Part>),
    /// Wait for BUSY high; `settle` first if a busy command just went.
    Busy { settle: bool, timeout: Millis },
    Set(Flag, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    Settling { until: Millis },
    Busy { deadline: Millis },
}

#[derive(Debug)]
struct Job {
    steps: VecDeque<Step>,
    wait: Option<Wait>,
    done: fn(Result<(), Fault>) -> Done,
}

#[derive(Debug, Default)]
pub struct Epaper {
    queue: VecDeque<Request>,
    job: Option<Job>,
    init: bool,
    power: bool,
    known: bool,
    finished: VecDeque<Done>,
}

impl Epaper {
    /// Nothing known about the panel: the first draw must be `Full`.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn submit(&mut self, request: Request) {
        self.queue.push_back(request);
    }

    /// The next finished request's result, in request order.
    pub fn take_done(&mut self) -> Option<Done> {
        self.finished.pop_front()
    }

    /// Whether the panel's contents are known (a partial draw is possible).
    pub fn contents_known(&self) -> bool {
        self.known
    }

    pub fn next(&mut self, now: Millis) -> Next {
        loop {
            if self.job.is_none() && !self.start_job() {
                return Next::Idle;
            }
            let job = self.job.as_mut().expect("job");
            match job.steps.front().cloned() {
                None => {
                    let done = job.done;
                    self.job = None;
                    self.finished.push_back(done(Ok(())));
                }
                Some(Step::Set(flag, on)) => {
                    job.steps.pop_front();
                    self.set(flag, on);
                }
                Some(Step::Tx(parts)) => {
                    return Next::Transaction(Transaction { device: Device::Epaper, clock: CLOCK, parts });
                }
                Some(Step::Busy { settle, timeout }) => {
                    let wait = *job.wait.get_or_insert(if settle {
                        Wait::Settling { until: now + SETTLE_MS }
                    } else {
                        Wait::Busy { deadline: now + timeout }
                    });
                    return match wait {
                        Wait::Settling { until } => Next::WaitUntil(until),
                        Wait::Busy { deadline } => Next::WaitPin { pin: Input::EpdBusy, high: true, deadline },
                    };
                }
            }
        }
    }

    pub fn advance(&mut self, outcome: Outcome, now: Millis) {
        let Some(job) = self.job.as_mut() else { return };
        match outcome {
            Outcome::Tx(Ok(_)) => {
                job.steps.pop_front();
            }
            Outcome::Tx(Err(e)) => self.fail(Fault::Bus(e)),
            Outcome::Waited => match job.wait {
                Some(Wait::Settling { .. }) => {
                    let Some(Step::Busy { timeout, .. }) = job.steps.front() else { unreachable!("settling outside a busy step") };
                    job.wait = Some(Wait::Busy { deadline: now + timeout });
                }
                _ => {
                    job.wait = None;
                    job.steps.pop_front();
                }
            },
            Outcome::TimedOut => self.fail(Fault::Timeout),
        }
    }

    /// Take the next request off the queue; refused ones finish at once.
    fn start_job(&mut self) -> bool {
        while let Some(request) = self.queue.pop_front() {
            match self.plan(request) {
                Ok(job) => {
                    self.job = Some(job);
                    return true;
                }
                Err(done) => self.finished.push_back(done),
            }
        }
        false
    }

    /// The steps for `request`, given the current controller state.
    fn plan(&self, request: Request) -> Result<Job, Done> {
        let mut p = Plan { steps: VecDeque::new(), init: self.init, power: self.power };
        // Settle first even here: a failed job may have just sent a busy
        // command whose BUSY hasn't fallen yet.
        p.steps.push_back(Step::Busy { settle: true, timeout: READY_TIMEOUT_MS });
        let done: fn(Result<(), Fault>) -> Done = match request {
            Request::Partial { window, data } => {
                if !window.is_valid() || data.len() != window.data_len() {
                    return Err(Done::Drawn(Err(Fault::Invalid)));
                }
                if !self.known {
                    return Err(Done::Drawn(Err(Fault::ContentsUnknown)));
                }
                p.write_window(0x13, window, &data);
                p.ensure_init();
                p.cmd(0x91);
                p.cmd_with(0x90, window.params());
                p.cmd_with(0xE0, vec![0x02]); // cascade setting: TSFIX
                p.cmd_with(0xE5, vec![0x79]); // forced temperature: fast partial
                p.cmd_with(0x50, vec![0xD7]); // VCOM / data interval
                p.ensure_power();
                p.refresh(PARTIAL_TIMEOUT_MS);
                p.cmd(0x92);
                p.write_window(0x10, window, &data);
                Done::Drawn
            }
            Request::Full { data } => {
                if data.len() != BUF_LEN {
                    return Err(Done::Drawn(Err(Fault::Invalid)));
                }
                p.ensure_init();
                p.cmd(0x10);
                p.data(&data);
                p.cmd(0x13);
                p.data(&data);
                p.cmd_with(0xE0, vec![0x02]);
                p.cmd_with(0xE5, vec![0x5A]); // forced temperature: fast full
                p.cmd_with(0x50, vec![0x97]);
                p.ensure_power();
                p.refresh(FULL_TIMEOUT_MS);
                p.steps.push_back(Step::Set(Flag::Known, true));
                Done::Drawn
            }
            Request::PowerOff => {
                if p.power {
                    p.cmd(0x02);
                    p.steps.push_back(Step::Busy { settle: true, timeout: POWER_TIMEOUT_MS });
                    p.set(Flag::Power, false);
                }
                Done::PoweredOff
            }
        };
        Ok(Job { steps: p.steps, wait: None, done })
    }

    fn set(&mut self, flag: Flag, on: bool) {
        match flag {
            Flag::Init => self.init = on,
            Flag::Power => self.power = on,
            Flag::Known => self.known = on,
        }
    }

    /// End the job with `fault`. The controller's state is now unknown:
    /// re-initialise, power on again, and only a full draw may follow.
    fn fail(&mut self, fault: Fault) {
        if let Some(job) = self.job.take() {
            self.finished.push_back((job.done)(Err(fault)));
        }
        self.init = false;
        self.power = false;
        self.known = false;
    }
}

/// Builds a job's steps, tracking the controller state they lead to.
struct Plan {
    steps: VecDeque<Step>,
    init: bool,
    power: bool,
}

impl Plan {
    fn cmd(&mut self, c: u8) {
        self.steps.push_back(Step::Tx(vec![Part::Command(vec![c])]));
    }

    fn cmd_with(&mut self, c: u8, params: Vec<u8>) {
        self.cmd(c);
        self.steps.push_back(Step::Tx(vec![Part::Write(params)]));
    }

    fn data(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(CHUNK) {
            self.steps.push_back(Step::Tx(vec![Part::Write(chunk.to_vec())]));
        }
    }

    fn set(&mut self, flag: Flag, on: bool) {
        match flag {
            Flag::Init => self.init = on,
            Flag::Power => self.power = on,
            Flag::Known => {}
        }
        self.steps.push_back(Step::Set(flag, on));
    }

    /// Soft reset then panel setting. The soft reset also switches the
    /// drive voltages off (UC8253 PSR RST_N: "Booster OFF").
    fn ensure_init(&mut self) {
        if self.init {
            return;
        }
        self.cmd_with(0x00, vec![0x1E, 0x0D]); // soft reset
        self.steps.push_back(Step::Busy { settle: true, timeout: RESET_TIMEOUT_MS });
        self.cmd_with(0x00, vec![0x1F, 0x0D]); // KW mode, LUT from OTP
        self.set(Flag::Init, true);
        self.set(Flag::Power, false);
    }

    fn ensure_power(&mut self) {
        if self.power {
            return;
        }
        self.cmd(0x04);
        self.steps.push_back(Step::Busy { settle: true, timeout: POWER_TIMEOUT_MS });
        self.set(Flag::Power, true);
    }

    /// Display refresh; the controller needs re-initialising afterwards
    /// (observed by GxEPD2 and our previous driver, reason unknown).
    fn refresh(&mut self, timeout: Millis) {
        self.cmd(0x12);
        self.steps.push_back(Step::Busy { settle: true, timeout });
        self.set(Flag::Init, false);
    }

    /// Write `data` into buffer `reg` (0x10 or 0x13) within `window`.
    fn write_window(&mut self, reg: u8, window: Window, data: &[u8]) {
        self.ensure_init();
        self.cmd(0x91); // partial mode in
        self.cmd_with(0x90, window.params());
        self.cmd(reg);
        self.data(data);
        self.cmd(0x92); // partial mode out
    }
}

#[cfg(test)]
pub(crate) mod tests;
