//! The kernel on the digital twin: the real `Spi` scheduler over
//! `MockWire` with `SimEpaper`, and mock drivers for the programs.

use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::battery::tests::MockBatteryDriver;
use crate::charger::tests::MockChargerDriver;
use crate::credentials::tests::MockCredentialStore;
use crate::email::tests::MockSmtpStreamFactory;
use crate::framebuffer::BUF_LEN;
use crate::http::tests::MockHttpClient;
use crate::modem::tests::MockModem;
use crate::saved_networks::tests::MockNetworkStore;
use crate::spi::bus::Device;
use crate::spi::epaper::tests::sim::{Refresh, SimEpaper};
use crate::spi::scheduler::Spi;
use crate::spi::wire::tests::MockWire;
use crate::spi::wire::Clock;
use crate::wifi::tests::MockWifiDriver;

/// The spi task, simulated.
struct Twin {
    spi: Spi,
    wire: MockWire,
    sim: Rc<RefCell<SimEpaper>>,
    submitted: usize,
}

impl SpiPort for Twin {
    fn submit(&mut self, request: Request) -> RequestId {
        self.submitted += 1;
        self.spi.submit(request)
    }
}

struct Drivers {
    wifi: MockWifiDriver,
    http: MockHttpClient,
    saved: MockNetworkStore,
    smtp: MockSmtpStreamFactory,
    creds: MockCredentialStore,
    modem: MockModem,
    battery: MockBatteryDriver,
    charger: MockChargerDriver,
}

struct Rig {
    kernel: Kernel,
    twin: Twin,
    d: Drivers,
    effects: Vec<Effect>,
}

impl Rig {
    fn new() -> Self {
        let mut wire = MockWire::new();
        let sim = wire.attach_shared(Device::Epaper, SimEpaper::new());
        let mut rig = Rig {
            kernel: Kernel::new(),
            twin: Twin { spi: Spi::new(), wire, sim, submitted: 0 },
            d: Drivers {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
                modem: MockModem::new(),
                battery: MockBatteryDriver::new(),
                charger: MockChargerDriver::new(),
            },
            effects: Vec::new(),
        };
        rig.event(Event::Deadline); // the main loop's first pass
        rig.settle();
        rig
    }

    fn now(&self) -> Millis {
        self.twin.wire.now()
    }

    fn event(&mut self, e: Event) {
        let now = self.now();
        let d = &mut self.d;
        let mut ctx = ExecContext {
            uptime_secs: now / 1000,
            wifi: &mut d.wifi,
            http: &mut d.http,
            saved_networks: &mut d.saved,
            smtp: &mut d.smtp,
            credentials: &mut d.creds,
            modem: &mut d.modem,
            battery: &mut d.battery,
            charger: &mut d.charger,
        };
        let effects = self.kernel.handle(e, now, &mut ctx, &mut self.twin);
        self.effects.extend(effects);
    }

    /// Run the spi task and the kernel's deadlines until the display is
    /// idle and no deadline is due within `quiet_ms`.
    fn settle_for(&mut self, quiet_ms: Millis) {
        let mut out = Vec::new();
        let limit = self.now() + 60_000;
        loop {
            assert!(self.now() < limit, "kernel never settled");
            self.twin.spi.step(&mut self.twin.wire, &mut out);
            let responses: Vec<_> = out.drain(..).collect();
            for (id, r) in responses {
                self.event(Event::Spi(id, r));
            }
            let now = self.now();
            if self.kernel.next_deadline().is_some_and(|d| now >= d) {
                self.event(Event::Deadline);
                continue;
            }
            let spi_wake = self.twin.spi.wake(now);
            let spi_busy = spi_wake.deadline.is_some() || !spi_wake.pins.is_empty();
            let deadline_soon = self.kernel.next_deadline().is_some_and(|d| d <= now + quiet_ms);
            if !spi_busy && self.kernel.display_idle() && !deadline_soon {
                return;
            }
            self.twin.wire.set_now(now + 1);
        }
    }

    /// Settle, but leave the panel's idle power-off for later.
    fn settle(&mut self) {
        self.settle_for(0);
    }

    fn keys(&mut self, text: &str) {
        for c in text.chars() {
            self.event(Event::Key(if c == '\n' { KeyEvent::Enter } else { KeyEvent::Char(c) }));
            self.settle();
        }
    }

    /// The panel shows exactly the framebuffer.
    fn assert_panel_shows_framebuffer(&self) {
        let sim = self.twin.sim.borrow();
        let wanted: Vec<u8> = (0..BUF_LEN).map(|i| self.kernel.fb().desired_byte(i)).collect();
        assert_eq!(sim.screen(), &wanted[..]);
        assert!(sim.screen_valid());
    }
}

#[test]
fn boot_draws_the_status_bar_and_prompt_with_one_full_refresh() {
    let rig = Rig::new();
    rig.assert_panel_shows_framebuffer();
    assert_eq!(rig.twin.sim.borrow().refreshes, vec![Refresh::Full]);
    assert!(rig.kernel.fb().desired_byte(0) != 0xFF, "status bar is black");
}

#[test]
fn typing_a_command_shows_its_output_with_small_partial_refreshes() {
    let mut rig = Rig::new();
    rig.keys("echo hi\n");
    rig.assert_panel_shows_framebuffer();
    let refreshes = rig.twin.sim.borrow().refreshes.clone();
    assert!(refreshes[1..].iter().all(|r| *r == Refresh::Partial), "{refreshes:?}");
    // Every key is one draw; nothing redraws unchanged pixels.
    assert!(rig.twin.submitted <= 1 + 8, "{} draws", rig.twin.submitted);
}

#[test]
fn the_panel_powers_off_after_being_idle() {
    let mut rig = Rig::new();
    assert!(rig.twin.sim.borrow().powered());
    rig.settle_for(PANEL_IDLE_MS + 10);
    assert!(!rig.twin.sim.borrow().powered());
    assert_eq!(rig.kernel.next_deadline(), None, "nothing more to do");
}

#[test]
fn locking_shows_the_lockscreen_powers_off_and_ignores_keys() {
    let mut rig = Rig::new();
    rig.keys("ab");
    rig.event(Event::Key(KeyEvent::Lock));
    assert_eq!(rig.effects, vec![Effect::Locked]);
    assert!(rig.kernel.is_locked());
    rig.settle();
    assert!(rig.kernel.display_idle());
    assert!(!rig.twin.sim.borrow().powered(), "powered off at once, not after the idle delay");
    rig.assert_panel_shows_framebuffer();
    let lockscreen = rig.twin.sim.borrow().screen().to_vec();
    let draws = rig.twin.submitted;
    rig.keys("xyz\n");
    assert_eq!(rig.twin.submitted, draws, "keys while locked draw nothing");
    assert_eq!(rig.twin.sim.borrow().screen(), &lockscreen[..]);
    rig.event(Event::Key(KeyEvent::Lock));
    assert_eq!(rig.effects, vec![Effect::Locked, Effect::Unlocked]);
    rig.settle();
    rig.assert_panel_shows_framebuffer();
    assert_eq!(rig.twin.sim.borrow().refreshes.last(), Some(&Refresh::Full), "clean full refresh on unlock");
    // The typed "ab" is still on the input line; the locked keys aren't.
    rig.keys("\n");
}

#[test]
fn power_off_waits_for_the_goodbye_and_the_panel_power_off() {
    let mut rig = Rig::new();
    rig.keys("power off\n");
    assert_eq!(rig.effects, vec![Effect::PowerOff]);
    rig.settle();
    assert!(rig.kernel.display_idle());
    assert!(!rig.twin.sim.borrow().powered());
    rig.assert_panel_shows_framebuffer();
}

#[test]
fn a_failed_draw_is_redrawn_in_full() {
    let mut rig = Rig::new();
    rig.twin.sim.borrow_mut().tolerate_cut_off = true;
    rig.twin.wire.fail_transfer_after(4);
    rig.keys("q");
    rig.settle();
    rig.assert_panel_shows_framebuffer();
    let refreshes = rig.twin.sim.borrow().refreshes.clone();
    assert_eq!(refreshes.iter().filter(|r| **r == Refresh::Full).count(), 2, "{refreshes:?}");
}
