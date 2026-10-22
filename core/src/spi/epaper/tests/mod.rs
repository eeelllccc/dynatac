//! E-paper protocol tests: the real `Bus` over `MockWire` with
//! `SimEpaper` attached, driven by a one-device stand-in for the scheduler.

pub(crate) mod sim;

use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::framebuffer::BYTES_PER_ROW;
use crate::spi::bus::Bus;
use crate::spi::wire::tests::MockWire;
use crate::spi::wire::Clock;
use sim::{Refresh, SimEpaper};

struct Rig {
    bus: Bus,
    wire: MockWire,
    sim: Rc<RefCell<SimEpaper>>,
    epd: Epaper,
    now: Millis,
    done: Vec<Done>,
}

impl Rig {
    fn new() -> Self {
        Self::with(SimEpaper::new())
    }

    fn with(sim: SimEpaper) -> Self {
        let mut wire = MockWire::new();
        let sim = wire.attach_shared(Device::Epaper, sim);
        let mut bus = Bus::new();
        bus.bring_up(&mut wire).unwrap();
        Rig { bus, wire, sim, epd: Epaper::new(), now: 0, done: Vec::new() }
    }

    fn tick(&mut self) {
        self.wire.set_now(self.wire.now() + 1);
        self.now = self.wire.now();
    }

    /// Drive the protocol until it is idle (or `limit_ms` passes).
    fn run(&mut self, limit_ms: Millis) {
        let end = self.now + limit_ms;
        while self.now < end {
            match self.epd.next(self.now) {
                Next::Idle => break,
                Next::Transaction(t) => {
                    let r = self.bus.run(&mut self.wire, &t);
                    self.now = self.wire.now(); // transfers take time
                    self.epd.advance(Outcome::Tx(r), self.now);
                }
                Next::WaitUntil(t) => {
                    while self.now < t {
                        self.tick();
                    }
                    self.epd.advance(Outcome::Waited, self.now);
                }
                Next::WaitPin { pin, high, deadline } => loop {
                    if self.bus.read(&mut self.wire, pin) == high {
                        self.epd.advance(Outcome::Waited, self.now);
                        break;
                    }
                    if self.now >= deadline {
                        self.epd.advance(Outcome::TimedOut, self.now);
                        break;
                    }
                    self.tick();
                },
            }
            while let Some(d) = self.epd.take_done() {
                self.done.push(d);
            }
        }
        while let Some(d) = self.epd.take_done() {
            self.done.push(d);
        }
    }

    fn submit_and_run(&mut self, r: Request) -> Done {
        let before = self.done.len();
        self.epd.submit(r);
        self.run(60_000);
        assert_eq!(self.done.len(), before + 1, "one Done per request");
        self.done.last().unwrap().clone()
    }

    fn full(&mut self, data: &[u8]) -> Done {
        self.submit_and_run(Request::Full { data: data.to_vec() })
    }

    fn partial(&mut self, window: Window, data: &[u8]) -> Done {
        self.submit_and_run(Request::Partial { window, data: data.to_vec() })
    }
}

fn screen_of(byte: u8) -> Vec<u8> {
    vec![byte; BUF_LEN]
}

/// Copy `data` into `screen` at `window`, as the panel should show it.
fn paint(screen: &mut [u8], window: Window, data: &[u8]) {
    let wb = window.w as usize / 8;
    for row in 0..window.h as usize {
        let at = (window.y as usize + row) * BYTES_PER_ROW + window.x as usize / 8;
        screen[at..at + wb].copy_from_slice(&data[row * wb..(row + 1) * wb]);
    }
}

const OK: Done = Done::Drawn(Ok(()));
const WIN: Window = Window { x: 16, y: 40, w: 64, h: 20 };

fn win_data(byte: u8) -> Vec<u8> {
    vec![byte; WIN.data_len()]
}

#[test]
fn the_first_draw_must_be_full() {
    let mut rig = Rig::new();
    assert_eq!(rig.partial(WIN, &win_data(0)), Done::Drawn(Err(Fault::ContentsUnknown)));
    assert!(rig.sim.borrow().log.is_empty(), "nothing sent");
}

#[test]
fn a_full_draw_shows_the_image_and_leaves_both_buffers_equal() {
    let mut rig = Rig::new();
    let image: Vec<u8> = (0..BUF_LEN).map(|i| i as u8).collect();
    assert_eq!(rig.full(&image), OK);
    let sim = rig.sim.borrow();
    assert_eq!(sim.screen(), &image[..]);
    assert_eq!(sim.old(), &image[..]);
    assert!(sim.screen_valid());
    assert_eq!(sim.refreshes, vec![Refresh::Full]);
    assert!(rig.epd.contents_known());
}

#[test]
fn a_partial_draw_updates_only_its_window_and_keeps_previous_equal_to_screen() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0xFF));
    assert_eq!(rig.partial(WIN, &win_data(0x0F)), OK);
    let mut expected = screen_of(0xFF);
    paint(&mut expected, WIN, &win_data(0x0F));
    let sim = rig.sim.borrow();
    assert_eq!(sim.screen(), &expected[..]);
    assert_eq!(sim.old(), &expected[..]);
    assert!(sim.screen_valid());
    assert_eq!(sim.refreshes, vec![Refresh::Full, Refresh::Partial]);
}

/// The command sequence of a partial draw, as GxEPD2 and our previous
/// driver send it (`docs/spi-research.md` §2).
#[test]
fn partial_draw_command_sequence_is_the_proven_one() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0xFF));
    rig.sim.borrow_mut().log.clear();
    rig.partial(WIN, &win_data(0));
    let p = WIN.params();
    let expected: Vec<(u8, Vec<u8>)> = vec![
        // The full draw's refresh left the controller needing init.
        (0x00, vec![0x1E, 0x0D]),
        (0x00, vec![0x1F, 0x0D]),
        (0x91, vec![]),
        (0x90, p.clone()),
        (0x13, vec![]),
        (0x92, vec![]),
        (0x91, vec![]),
        (0x90, p.clone()),
        (0xE0, vec![0x02]),
        (0xE5, vec![0x79]),
        (0x50, vec![0xD7]),
        (0x04, vec![]), // the soft reset switched the voltages off
        (0x12, vec![]),
        (0x92, vec![]),
        (0x00, vec![0x1E, 0x0D]),
        (0x00, vec![0x1F, 0x0D]),
        (0x91, vec![]),
        (0x90, p),
        (0x10, vec![]),
        (0x92, vec![]),
    ];
    assert_eq!(rig.sim.borrow().log, expected);
}

#[test]
fn window_parameters_are_byte_aligned_and_inclusive() {
    let w = Window { x: 16, y: 300, w: 64, h: 20 };
    assert_eq!(w.params(), vec![16, 79, 0x01, 0x2C, 0x01, 0x3F, 0x01]);
}

#[test]
fn image_data_goes_in_chunks_of_at_most_chunk_bytes() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0xAA));
    let sizes: Vec<usize> = rig.wire.frames().iter().filter(|f| f.device == Device::Epaper).map(|f| f.bytes.len()).collect();
    assert!(sizes.iter().all(|&n| n <= CHUNK), "{sizes:?}");
    // 9600 bytes per buffer → 4092 + 4092 + 1416, twice.
    assert_eq!(sizes.iter().filter(|&&n| n == CHUNK).count(), 4);
}

#[test]
fn every_transaction_runs_at_the_protocol_clock() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0));
    for f in rig.wire.frames().iter().filter(|f| f.device == Device::Epaper) {
        assert!(f.bytes.iter().all(|b| b.clock == CLOCK));
    }
}

#[test]
fn invalid_requests_are_refused_without_touching_the_panel() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0xFF));
    rig.sim.borrow_mut().log.clear();
    let bad = [
        (Window { x: 3, y: 0, w: 8, h: 1 }, 1),    // x not aligned
        (Window { x: 0, y: 0, w: 12, h: 1 }, 2),   // w not aligned
        (Window { x: 232, y: 0, w: 16, h: 1 }, 2), // off the right edge
        (Window { x: 0, y: 319, w: 8, h: 2 }, 2),  // off the bottom
        (Window { x: 0, y: 0, w: 0, h: 1 }, 0),    // empty
        (Window { x: 0, y: 0, w: 8, h: 2 }, 3),    // wrong data length
    ];
    for (window, len) in bad {
        assert_eq!(rig.partial(window, &vec![0; len]), Done::Drawn(Err(Fault::Invalid)), "{window:?}");
    }
    assert_eq!(rig.full(&[0; 10]), Done::Drawn(Err(Fault::Invalid)));
    assert!(rig.sim.borrow().log.is_empty());
}

#[test]
fn busy_falling_late_is_waited_for() {
    let mut sim = SimEpaper::new();
    sim.busy_lag_ms = SETTLE_MS; // the latest our settle covers
    let mut rig = Rig::with(sim);
    assert_eq!(rig.full(&screen_of(0xFF)), OK);
    assert_eq!(rig.partial(WIN, &win_data(0)), OK);
    assert!(rig.sim.borrow().screen_valid());
}

#[test]
fn busy_stuck_low_times_out_and_then_needs_a_full_draw() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0xFF));
    rig.sim.borrow_mut().stuck = Some(false);
    let start = rig.now;
    assert_eq!(rig.partial(WIN, &win_data(0)), Done::Drawn(Err(Fault::Timeout)));
    assert!(rig.now - start <= READY_TIMEOUT_MS + 1);
    assert!(!rig.epd.contents_known());
    rig.sim.borrow_mut().stuck = None;
    assert_eq!(rig.partial(WIN, &win_data(0)), Done::Drawn(Err(Fault::ContentsUnknown)));
    assert_eq!(rig.full(&screen_of(0x55)), OK);
    assert_eq!(rig.sim.borrow().screen(), &screen_of(0x55)[..]);
}

#[test]
fn a_refresh_that_never_finishes_times_out() {
    let mut sim = SimEpaper::new();
    sim.full_ms = FULL_TIMEOUT_MS * 3;
    let mut rig = Rig::with(sim);
    assert_eq!(rig.full(&screen_of(0)), Done::Drawn(Err(Fault::Timeout)));
    assert!(!rig.epd.contents_known());
}

#[test]
fn a_bus_failure_mid_draw_is_reported_and_then_needs_a_full_draw() {
    let mut sim = SimEpaper::new();
    sim.tolerate_cut_off = true;
    let mut rig = Rig::with(sim);
    rig.full(&screen_of(0xFF));
    rig.wire.fail_transfer_after(5);
    let d = rig.partial(WIN, &win_data(0));
    assert!(matches!(d, Done::Drawn(Err(Fault::Bus(_)))), "{d:?}");
    assert_eq!(rig.partial(WIN, &win_data(0)), Done::Drawn(Err(Fault::ContentsUnknown)));
    assert_eq!(rig.full(&screen_of(0x33)), OK);
    assert!(rig.sim.borrow().screen_valid());
}

#[test]
fn power_off_switches_the_voltages_off_and_the_next_draw_powers_on() {
    let mut rig = Rig::new();
    rig.full(&screen_of(0xFF));
    assert!(rig.sim.borrow().powered());
    assert_eq!(rig.submit_and_run(Request::PowerOff), Done::PoweredOff(Ok(())));
    assert!(!rig.sim.borrow().powered());
    // Already off: nothing to send.
    let sent = rig.sim.borrow().log.len();
    assert_eq!(rig.submit_and_run(Request::PowerOff), Done::PoweredOff(Ok(())));
    assert_eq!(rig.sim.borrow().log.len(), sent);
    assert_eq!(rig.partial(WIN, &win_data(0)), OK);
    assert!(rig.sim.borrow().screen_valid());
}

#[test]
fn queued_requests_finish_in_order_one_done_each() {
    let mut rig = Rig::new();
    rig.epd.submit(Request::Partial { window: WIN, data: win_data(0) }); // refused: unknown
    rig.epd.submit(Request::Full { data: screen_of(0xFF) });
    rig.epd.submit(Request::Partial { window: WIN, data: win_data(1) });
    rig.epd.submit(Request::PowerOff);
    rig.run(60_000);
    assert_eq!(
        rig.done,
        vec![Done::Drawn(Err(Fault::ContentsUnknown)), OK, OK, Done::PoweredOff(Ok(()))]
    );
}

/// Seeded random requests under random faults (wire failures, BUSY stuck
/// for a while, late BUSY, slow refreshes). Checks: no simulator panic
/// (no byte while busy, no malformed command), one Done per request in
/// order, and after a final fault-free full draw plus partials, the screen
/// shows exactly what was drawn and every pixel was driven correctly.
#[test]
fn random_requests_under_faults() {
    let (mut timeouts, mut bus_faults, mut ok) = (0, 0, 0);
    for seed in 1..=60u64 {
        let mut rng = seed;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let mut sim = SimEpaper::new();
        sim.busy_lag_ms = next(2);
        sim.tolerate_cut_off = true;
        let mut rig = Rig::with(sim);
        for _ in 0..30 {
            match next(6) {
                0 => rig.wire.fail_transfer_after(next(40) as usize),
                1 => rig.sim.borrow_mut().stuck = Some(false),
                2 => rig.sim.borrow_mut().stuck = None,
                3 => rig.sim.borrow_mut().partial_ms = 300 + next(2_000),
                _ => {}
            }
            let request = match next(4) {
                0 => Request::Full { data: screen_of(next(256) as u8) },
                1 => Request::PowerOff,
                _ => {
                    let w = Window { x: 8 * next(20) as u16, y: next(300) as u16, w: 8 * (1 + next(10) as u16), h: 1 + next(20) as u16 };
                    Request::Partial { window: w, data: vec![next(256) as u8; w.data_len()] }
                }
            };
            let is_power_off = request == Request::PowerOff;
            let d = rig.submit_and_run(request);
            assert_eq!(matches!(d, Done::PoweredOff(_)), is_power_off, "seed {seed}");
            match d {
                Done::Drawn(Err(Fault::Timeout)) | Done::PoweredOff(Err(Fault::Timeout)) => timeouts += 1,
                Done::Drawn(Err(Fault::Bus(_))) | Done::PoweredOff(Err(Fault::Bus(_))) => bus_faults += 1,
                Done::Drawn(Ok(())) => ok += 1,
                _ => {}
            }
        }
        // Clear the faults; then the panel must come right.
        rig.wire.fail_transfer_after(usize::MAX);
        {
            let mut sim = rig.sim.borrow_mut();
            sim.stuck = None;
            sim.partial_ms = 650;
        }
        let mut expected = screen_of(0xF0);
        assert_eq!(rig.full(&expected), OK, "seed {seed}");
        for k in 0..3u16 {
            let w = Window { x: 8 * k, y: 10 * k, w: 24, h: 5 };
            let data = vec![k as u8; w.data_len()];
            assert_eq!(rig.partial(w, &data), OK, "seed {seed}");
            paint(&mut expected, w, &data);
        }
        let sim = rig.sim.borrow();
        assert_eq!(sim.screen(), &expected[..], "seed {seed}");
        assert_eq!(sim.old(), &expected[..], "seed {seed}");
        assert!(sim.screen_valid(), "seed {seed}");
    }
    // The faults were really exercised.
    assert!(timeouts > 20 && bus_faults > 20 && ok > 100, "timeouts {timeouts}, bus {bus_faults}, ok {ok}");
}

#[test]
fn busy_stuck_high_never_hangs() {
    // A BUSY line stuck high can't be told from a fast panel: commands
    // sent mid-refresh are ignored and the image may be wrong. All the
    // protocol can promise is to keep answering.
    let mut sim = SimEpaper::new();
    sim.stuck = Some(true);
    let mut rig = Rig::with(sim);
    assert_eq!(rig.full(&screen_of(0)), OK);
    assert_eq!(rig.partial(WIN, &win_data(1)), OK);
    assert_eq!(rig.submit_and_run(Request::PowerOff), Done::PoweredOff(Ok(())));
}
