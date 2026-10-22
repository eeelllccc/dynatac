use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::framebuffer::BUF_LEN;
use crate::spi::bus::{Part, Transaction};
use crate::spi::epaper::tests::sim::SimEpaper;
use crate::spi::epaper::{Done, Fault, Window};
use crate::spi::wire::tests::MockWire;
use crate::spi::wire::{Clock, Hz};

/// A device that always has more transactions ready, until its work runs
/// out. Stands in for SD / LoRa to test the scheduling.
struct MockProtocol {
    device: Device,
    work: VecDeque<Transaction>,
}

impl MockProtocol {
    fn busy(device: Device, n: usize, bytes: usize, clock: Hz) -> Self {
        let t = Transaction { device, clock, parts: vec![Part::Write(vec![0xA5; bytes])] };
        MockProtocol { device, work: std::iter::repeat_n(t, n).collect() }
    }
}

impl DeviceProtocol for MockProtocol {
    fn submit(&mut self, _: Request) {
        unreachable!("{:?} takes no requests", self.device)
    }
    fn next(&mut self, _: Millis) -> Next {
        self.work.front().cloned().map_or(Next::Idle, Next::Transaction)
    }
    fn advance(&mut self, outcome: Outcome, _: Millis) {
        assert!(matches!(outcome, Outcome::Tx(Ok(_))));
        self.work.pop_front();
    }
    fn take_done(&mut self) -> Option<Response> {
        None
    }
}

struct Rig {
    spi: Spi,
    wire: MockWire,
    sim: Rc<RefCell<SimEpaper>>,
    out: Vec<(RequestId, Response)>,
}

impl Rig {
    fn new(spi: Spi) -> Self {
        Self::with_sim(spi, SimEpaper::new())
    }

    fn with_sim(spi: Spi, sim: SimEpaper) -> Self {
        let mut wire = MockWire::new();
        let sim = wire.attach_shared(Device::Epaper, sim);
        Rig { spi, wire, sim, out: Vec::new() }
    }

    /// Run like the spi task: step, then sleep until the wake deadline or a
    /// watched pin reaches its level (checked each millisecond).
    fn drive(&mut self, limit_ms: Millis) {
        let end = self.wire.now() + limit_ms;
        loop {
            self.spi.step(&mut self.wire, &mut self.out);
            let wake = self.spi.wake(self.wire.now());
            if wake.deadline.is_none() && wake.pins.is_empty() {
                return;
            }
            loop {
                let now = self.wire.now();
                assert!(now < end, "still busy after {limit_ms} ms");
                if wake.deadline.is_some_and(|d| now >= d) {
                    break;
                }
                if wake.pins.iter().any(|&(pin, high)| self.wire.get(pin) == high) {
                    break;
                }
                self.wire.set_now(now + 1);
            }
        }
    }

    /// One step, then the wake it asks for.
    fn step(&mut self) -> Wake {
        self.spi.step(&mut self.wire, &mut self.out);
        self.spi.wake(self.wire.now())
    }
}

fn full(byte: u8) -> Request {
    Request::Epaper(epaper::Request::Full { data: vec![byte; BUF_LEN] })
}

fn partial(byte: u8) -> Request {
    let window = Window { x: 0, y: 0, w: 16, h: 4 };
    Request::Epaper(epaper::Request::Partial { window, data: vec![byte; window.data_len()] })
}

const DRAWN: Response = Response::Epaper(Done::Drawn(Ok(())));

#[test]
fn the_first_step_brings_the_bus_up_then_draws() {
    let mut rig = Rig::new(Spi::new());
    let id = rig.spi.submit(full(0x5A));
    rig.drive(10_000);
    assert!(rig.wire.sd_spi_mode(), "bus brought up first");
    assert_eq!(rig.out, vec![(id, DRAWN)]);
    assert_eq!(rig.sim.borrow().screen(), &vec![0x5A; BUF_LEN][..]);
}

#[test]
fn responses_carry_their_ids_in_order() {
    let mut rig = Rig::new(Spi::new());
    let a = rig.spi.submit(partial(1)); // refused: contents unknown
    let b = rig.spi.submit(full(0xFF));
    let c = rig.spi.submit(partial(2));
    rig.drive(10_000);
    assert_eq!(
        rig.out,
        vec![(a, Response::Epaper(Done::Drawn(Err(Fault::ContentsUnknown)))), (b, DRAWN), (c, DRAWN)]
    );
}

#[test]
fn while_refreshing_it_waits_on_busy_and_when_idle_on_nothing() {
    let mut rig = Rig::new(Spi::new());
    rig.spi.submit(full(0));
    // Step at each deadline until the protocol waits on BUSY.
    let wake = loop {
        let w = rig.step();
        if !w.pins.is_empty() {
            break w;
        }
        let d = w.deadline.expect("a settle deadline before the first BUSY wait");
        assert!(d < 100, "no BUSY wait within 100 ms");
        rig.wire.set_now(d);
    };
    assert_eq!(wake.pins, vec![(Input::EpdBusy, true)]);
    assert!(wake.deadline.is_some_and(|d| d > rig.wire.now()));
    rig.drive(10_000);
    assert_eq!(rig.spi.wake(rig.wire.now()), Wake::default());
}

/// S1: with another device always ready, the e-paper and it alternate,
/// and the other device never waits more than one e-paper chunk.
#[test]
fn round_robin_bounds_every_device_wait() {
    // ≈ 0.2 ms each: 2000 keep the SD card busy for the whole draw.
    let sd = MockProtocol::busy(Device::SdCard, 2000, 512, Hz(20_000_000));
    let spi = Spi::with_devices(vec![(Device::Epaper, Box::new(Epaper::new())), (Device::SdCard, Box::new(sd))]);
    let mut rig = Rig::new(spi);
    rig.spi.submit(full(0x11));
    rig.drive(10_000);
    let frames: Vec<_> = rig.wire.frames().iter().filter(|f| f.device != Device::SdCard || f.bytes.len() == 512).collect();
    let last_sd = frames.iter().rposition(|f| f.device == Device::SdCard).unwrap();
    for pair in frames[..last_sd].windows(2) {
        assert!(
            !(pair[0].device == Device::Epaper && pair[1].device == Device::Epaper),
            "two e-paper transactions in a row while the SD card was waiting"
        );
    }
    let sd_starts: Vec<u64> = frames.iter().filter(|f| f.device == Device::SdCard).map(|f| f.start_ns).collect();
    assert_eq!(sd_starts.len(), 2000);
    let worst = sd_starts.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(worst <= 7_000_000, "SD waited {worst} ns between turns (S1: ≤ 7 ms)");
    assert!(worst > 6_000_000, "the worst gap is an e-paper chunk: {worst} ns");
    assert_eq!(rig.sim.borrow().screen(), &vec![0x11; BUF_LEN][..], "the e-paper still finished");
}

#[test]
fn a_request_for_a_device_with_no_protocol_gets_no_device() {
    let mut rig = Rig::new(Spi::with_devices(vec![]));
    let id = rig.spi.submit(full(0));
    rig.drive(1_000);
    assert_eq!(rig.out, vec![(id, Response::NoDevice)]);
}

#[test]
fn a_failed_bring_up_is_retried_later() {
    let mut rig = Rig::new(Spi::new());
    rig.wire.fail_transfer_after(0);
    let id = rig.spi.submit(full(0));
    assert_eq!(rig.step().deadline, Some(BRING_UP_RETRY_MS));
    let events = rig.wire.events().len();
    rig.wire.set_now(BRING_UP_RETRY_MS / 2);
    rig.step();
    assert_eq!(rig.wire.events().len(), events, "no retry before the deadline");
    rig.wire.set_now(BRING_UP_RETRY_MS);
    rig.drive(10_000);
    assert_eq!(rig.out, vec![(id, DRAWN)]);
}

#[test]
fn a_stuck_busy_times_out_through_the_wake_deadline() {
    let mut sim = SimEpaper::new();
    sim.stuck = Some(false);
    let mut rig = Rig::with_sim(Spi::new(), sim);
    let id = rig.spi.submit(full(0));
    rig.drive(epaper::READY_TIMEOUT_MS + 100);
    assert_eq!(rig.out, vec![(id, Response::Epaper(Done::Drawn(Err(Fault::Timeout))))]);
}

/// The kernel's display loop in miniature: draw into the framebuffer,
/// flush through the spi scheduler, end the flush with the response. The
/// simulated panel must end up showing exactly what was drawn, including
/// across a failed draw (which forces the next flush to be full).
#[test]
fn framebuffer_flushes_reach_the_panel_exactly() {
    use crate::font::FontSize;
    use crate::framebuffer::FrameBuffer;

    let mut sim = SimEpaper::new();
    sim.tolerate_cut_off = true;
    let mut rig = Rig::with_sim(Spi::new(), sim);
    let mut fb = FrameBuffer::new();
    let flush = |rig: &mut Rig, fb: &mut FrameBuffer| {
        while let Some(f) = fb.begin_flush() {
            rig.out.clear();
            rig.spi.submit(Request::Epaper(f.into()));
            rig.drive(20_000);
            let ok = rig.out == vec![(rig.out[0].0, DRAWN)];
            fb.end_flush(ok);
        }
    };
    for (i, text) in ["hello", "hello world", "hellp world", "", "bye"].iter().enumerate() {
        fb.fill_rect(0, 0, 240, 40, false);
        fb.draw_text(4, 4 + i as u16, FontSize::Medium.font(), text);
        if i == 2 {
            rig.wire.fail_transfer_after(3); // this draw fails part-way
        }
        flush(&mut rig, &mut fb);
        let shown = rig.sim.borrow().screen().to_vec();
        let wanted: Vec<u8> = (0..BUF_LEN).map(|j| fb.desired_byte(j)).collect();
        assert_eq!(shown, wanted, "after drawing {text:?}");
        assert!(rig.sim.borrow().screen_valid());
    }
    let refreshes = rig.sim.borrow().refreshes.clone();
    use crate::spi::epaper::tests::sim::Refresh::*;
    assert_eq!(refreshes.first(), Some(&Full), "boot: contents unknown");
    assert_eq!(refreshes.iter().filter(|r| **r == Full).count(), 2, "plus one after the failure: {refreshes:?}");
}
