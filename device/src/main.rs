mod battery;
mod charger;
mod http;
mod i2c_bus;
mod input_service;
pub mod keyboard;
mod modem;
mod nvs_credential_store;
mod nvs_network_store;
mod sleep;
mod smtp;
mod spi_service;
mod spi_wire;
mod wifi;

// The device's main loop is the kernel loop (docs/os-design.md): wait on
// the kernel inbox until the kernel's next deadline, hand the event to
// `Kernel::handle`, carry out the effects. Tasks own the hardware and post
// into the inbox:
//   - input task (src/input_service.rs): I2C bus — keyboard, fuel gauge,
//     charger; keys arrive as `Event::Key`.
//   - spi task (src/spi_service.rs): SPI2 — e-paper (SD and LoRa later);
//     responses arrive as `Event::Spi`.
// Still on the main thread until they move onto the kernel (migration
// steps 5–6): WiFi, HTTP, SMTP and the old modem driver, reached by the
// shell's programs through `ExecContext`. They block the loop while they
// run (a known, temporary break of os-design rule 4).

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::sleep;
use std::time::{Duration, Instant};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::{
    gpio::{OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    uart::{config::Config as UartConfig, UartDriver},
    units::Hertz,
};
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use dynatac_core::charger::ChargerDriver;
use dynatac_core::kernel::{Effect, Event, Kernel};
use dynatac_core::modem::Modem;
use dynatac_core::programs::ExecContext;
use dynatac_core::spi::Millis;
use http::EspHttpClient;
use input_service::ChargerProxy;
use modem::EspA7682EModem;
use nvs_credential_store::NvsCredentialStore;
use nvs_network_store::NvsNetworkStore;
use smtp::EspSmtpStreamFactory;
use spi_service::SpiHandle;
use spi_wire::Pins;
use wifi::EspWifiDriver;

/// Longest wait for the display to finish before powering off anyway.
const SHUTDOWN_DISPLAY_WAIT: Duration = Duration::from_secs(15);

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting dynatac OS");

    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let pins = peripherals.pins;
    let boot = Instant::now();
    let now = || boot.elapsed().as_millis() as Millis;

    let (to_kernel, inbox) = mpsc::channel();

    // --- Tasks that own hardware ----------------------------------------------
    let mut spi = SpiHandle::start(
        peripherals.spi2,
        Pins {
            sck: pins.gpio36,
            mosi: pins.gpio33,
            miso: pins.gpio47,
            epd_cs: pins.gpio34,
            epd_dc: pins.gpio35,
            epd_busy: pins.gpio37,
            sd_cs: pins.gpio48,
            lora_cs: pins.gpio3,
            lora_rst: pins.gpio4,
            lora_busy: pins.gpio6,
            lora_dio1: pins.gpio5,
            lora_en: pins.gpio46,
        },
        to_kernel.clone(),
    )
    .expect("spi task");

    let i2c = I2cDriver::new(peripherals.i2c0, pins.gpio13, pins.gpio14, &I2cConfig::new().baudrate(Hertz(100_000)))
        .expect("I2C0");
    let (mut battery, mut charger) = input_service::start(i2c, pins.gpio15, to_kernel.clone());
    drop(to_kernel); // only the tasks hold senders now

    // --- Drivers still on the main thread -------------------------------------
    let mut wifi = EspWifiDriver::new(peripherals.modem, sysloop, Some(nvs.clone()));
    let mut http_client = EspHttpClient::new();
    let mut saved_networks = NvsNetworkStore::new(nvs.clone());
    let mut credentials = NvsCredentialStore::new(nvs);
    let mut smtp_factory = EspSmtpStreamFactory::new();

    // A7682E 4G modem, old driver (off until `modem on`). Board pin map
    // (T-Deck-Pro utilities.h): POWER_EN GPIO41, PWRKEY GPIO40, modem RX =
    // GPIO10 (ESP TX), modem TX = GPIO11 (ESP RX), on UART1.
    let modem_uart = UartDriver::new(
        peripherals.uart1,
        pins.gpio10,
        pins.gpio11,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &UartConfig::new().baudrate(Hertz(115_200)),
    )
    .unwrap();
    let modem_pwrkey = PinDriver::output(pins.gpio40.downgrade_output()).unwrap();
    let modem_power_en = PinDriver::output(pins.gpio41.downgrade_output()).unwrap();
    let mut modem_driver = EspA7682EModem::new(modem_uart, modem_pwrkey, modem_power_en);

    // --- Kernel ----------------------------------------------------------------
    let mut kernel = Kernel::new();
    for msg in dynatac_core::startup::run_startup(&mut wifi, &mut saved_networks) {
        kernel.push_output(&msg);
    }

    log::info!("Ready — type on the keyboard");
    let mut next: Option<Event> = Some(Event::Deadline); // first pass: draw the boot screen
    loop {
        let event = match next.take() {
            Some(e) => e,
            None => wait(&inbox, kernel.next_deadline(), now()),
        };
        let mut ctx = ExecContext {
            uptime_secs: boot.elapsed().as_secs(),
            wifi: &mut wifi,
            http: &mut http_client,
            saved_networks: &mut saved_networks,
            smtp: &mut smtp_factory,
            credentials: &mut credentials,
            modem: &mut modem_driver,
            battery: &mut battery,
            charger: &mut charger,
        };
        let effects = kernel.handle(event, now(), &mut ctx, &mut spi);
        for effect in effects {
            match effect {
                Effect::Locked => {
                    log::info!("locked");
                    radios_off(&mut wifi, &mut modem_driver);
                }
                Effect::Unlocked => log::info!("unlocked"),
                Effect::PowerOff | Effect::ShipMode => {
                    let ship = effect == Effect::ShipMode;
                    finish_display(&mut kernel, &inbox, &mut spi, now);
                    radios_off(&mut wifi, &mut modem_driver);
                    if ship {
                        ship_mode(&mut charger);
                    }
                    log::info!("powering off (deep sleep)");
                    sleep::power_off();
                }
                Effect::Call(number) => {
                    log::info!("in call to {number}");
                    handle_call(&mut kernel, &inbox, &mut spi, &mut modem_driver, now);
                    kernel.terminal_mut().set_show_input(true);
                    kernel.redraw(&mut spi, now());
                }
            }
        }
        // Locked with the lockscreen drawn and the panel off: sleep until a
        // key (the input task's IRQ line is the wake source).
        if kernel.is_locked() && kernel.display_idle() {
            match inbox.try_recv() {
                Ok(e) => next = Some(e),
                Err(_) => {
                    if let Err(e) = sleep::enter() {
                        log::warn!("light sleep entry failed: {e:?}");
                    }
                }
            }
        }
    }
}

/// The next event, or `Deadline` once `deadline` passes.
fn wait(inbox: &Receiver<Event>, deadline: Option<Millis>, now: Millis) -> Event {
    let result = match deadline {
        Some(d) => inbox.recv_timeout(Duration::from_millis(d.saturating_sub(now))),
        None => inbox.recv().map_err(|_| RecvTimeoutError::Disconnected),
    };
    match result {
        Ok(e) => e,
        Err(RecvTimeoutError::Timeout) => Event::Deadline,
        Err(RecvTimeoutError::Disconnected) => panic!("every hardware task has stopped"),
    }
}

/// Let the display finish (final message drawn, panel powered off).
/// Keys are dropped: we're shutting down.
fn finish_display(kernel: &mut Kernel, inbox: &Receiver<Event>, spi: &mut SpiHandle, now: impl Fn() -> Millis) {
    let start = Instant::now();
    while !kernel.display_idle() && start.elapsed() < SHUTDOWN_DISPLAY_WAIT {
        match wait(inbox, kernel.next_deadline(), now()) {
            Event::Key(_) => {}
            e => kernel.handle_display(e, now(), spi),
        }
    }
}

fn radios_off(wifi: &mut EspWifiDriver, modem: &mut EspA7682EModem) {
    if let Err(e) = wifi.shutdown_for_sleep() {
        log::warn!("wifi shutdown failed: {e:?}");
    }
    if let Err(e) = modem.power_off() {
        log::warn!("modem power_off failed: {e:?}");
    }
}

/// BQ25896 ship mode: disconnects the battery; only USB restores power.
/// On USB the chip ignores it, so this never returns.
fn ship_mode(charger: &mut ChargerProxy) -> ! {
    log::info!("entering ship mode");
    if let Err(e) = charger.shutdown() {
        log::warn!("charger shutdown failed: {e:?}");
    }
    loop {
        sleep(Duration::from_secs(1));
    }
}

/// Block until the voice call ends (Enter, or the remote hangs up). The old
/// driver's in-call loop, now reading keys from the inbox and drawing
/// through the kernel. Temporary: `phone` moves onto `CallManager` in
/// migration step 5.
///
/// Shows a volume status line, updated in place, which u/d step up and
/// down; the level shown is read back from the modem.
fn handle_call(
    kernel: &mut Kernel,
    inbox: &Receiver<Event>,
    spi: &mut SpiHandle,
    modem: &mut EspA7682EModem,
    now: impl Fn() -> Millis,
) {
    use dynatac_core::call::{self, CallKey};
    use dynatac_core::modem::CallUrc;
    let call_start = Instant::now();
    let say = |kernel: &mut Kernel, spi: &mut SpiHandle, text: &str, replace: bool| {
        if replace {
            kernel.terminal_mut().replace_last_output(text);
        } else {
            kernel.terminal_mut().push_output(text);
        }
        kernel.redraw(spi, now());
    };

    let (mut volume, mut confirmed) = match query_volume(modem) {
        Some(v) => (v, true),
        None => (call::Volume::max(), false),
    };
    say(kernel, spi, &call::status_line(volume, confirmed), false);

    loop {
        // URCs before keys: a volume command drains the UART, so a pending
        // NO CARRIER must be consumed first.
        match modem.poll_urc() {
            Some(CallUrc::NoCarrier) => {
                let secs = call_start.elapsed().as_secs();
                say(kernel, spi, &format!("call ended ({:02}:{:02})", secs / 60, secs % 60), false);
                return;
            }
            Some(CallUrc::Busy) => {
                say(kernel, spi, "busy", false);
                return;
            }
            Some(CallUrc::Other(_)) | None => {}
        }
        let key = match inbox.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Key(k)) => Some(k),
            Ok(e) => {
                kernel.handle_display(e, now(), spi);
                None
            }
            Err(RecvTimeoutError::Timeout) => {
                kernel.handle_display(Event::Deadline, now(), spi);
                None
            }
            Err(RecvTimeoutError::Disconnected) => panic!("every hardware task has stopped"),
        };
        let target = match key.map(|k| call::classify_key(&k)) {
            Some(CallKey::HangUp) => break,
            Some(CallKey::VolumeUp) => Some(volume.up()),
            Some(CallKey::VolumeDown) => Some(volume.down()),
            Some(CallKey::Ignore) | None => None,
        };
        if let Some(target) = target {
            let r = modem.send_raw_with_timeout(&target.set_command(), Duration::from_secs(1));
            log::info!("{}: {:?}", target.set_command(), r);
            (volume, confirmed) = match query_volume(modem) {
                Some(v) => (v, true),
                None => (target, false),
            };
            say(kernel, spi, &call::status_line(volume, confirmed), true);
        }
    }

    // Enter pressed: hang up.
    let _ = modem.send_raw("AT+CHUP");
    let _ = modem.send_raw("ATH");
    let secs = call_start.elapsed().as_secs();
    say(kernel, spi, &format!("hung up ({:02}:{:02})", secs / 60, secs % 60), false);
}

/// Read the call volume back from the modem; `None` if that fails.
fn query_volume(modem: &mut EspA7682EModem) -> Option<dynatac_core::call::Volume> {
    use dynatac_core::call;
    let r = modem.send_raw_with_timeout(call::QUERY_COMMAND, Duration::from_secs(1));
    log::info!("{}: {:?}", call::QUERY_COMMAND, r);
    r.ok().and_then(|lines| call::parse_query(&lines))
}
