mod display;
mod http;
pub mod keyboard;
mod modem;
mod nvs_credential_store;
mod nvs_network_store;
mod smtp;
mod wifi;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, SpiConfig, SpiDeviceDriver, SpiDriver},
    uart::{config::Config as UartConfig, UartDriver},
    units::Hertz,
};

use display::Epd;
use dynatac_core::programs::ExecContext;
use dynatac_core::shell::{Shell, ShellAction};
use dynatac_core::terminal::{Terminal, TerminalAction};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use http::EspHttpClient;
use modem::EspA7682EModem;
use nvs_credential_store::NvsCredentialStore;
use nvs_network_store::NvsNetworkStore;
use smtp::EspSmtpStreamFactory;
use wifi::EspWifiDriver;
use keyboard::Keyboard;

/// Row height in pixels: 8px font + 2px gap.
const ROW_HEIGHT: u16 = 10;
/// Number of text columns (240px / 8px per glyph).
const TERM_COLS: usize = (display::WIDTH as usize) / 8;
/// Number of text rows (320px / 10px per row).
const TERM_ROWS: usize = (display::HEIGHT as usize) / ROW_HEIGHT as usize;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting dynatac OS");

    let peripherals = Peripherals::take().unwrap();
    let sysloop = EspSystemEventLoop::take().unwrap();
    let nvs = EspDefaultNvsPartition::take().unwrap();
    let pins = peripherals.pins;

    // --- Deselect other SPI devices on the shared bus ----------------------------
    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
    sd_cs.set_high().unwrap();

    // --- SPI bus for EPD ---------------------------------------------------------
    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        pins.gpio36,
        pins.gpio33,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &DriverConfig::new(),
    )
    .unwrap();

    let spi_device = SpiDeviceDriver::new(
        spi_driver,
        None::<AnyOutputPin>,
        &SpiConfig::new().baudrate(Hertz(10_000_000)),
    )
    .unwrap();

    let cs = PinDriver::output(pins.gpio34.downgrade_output()).unwrap();
    let dc = PinDriver::output(pins.gpio35.downgrade_output()).unwrap();
    let busy = PinDriver::input(pins.gpio37.downgrade()).unwrap();

    let mut epd = Epd::new(spi_device, cs, dc, busy);

    // --- I2C bus for keyboard ----------------------------------------------------
    let i2c = I2cDriver::new(
        peripherals.i2c0,
        pins.gpio13,
        pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    )
    .unwrap();

    let mut kb = Keyboard::new(i2c).unwrap();

    // --- Drivers -----------------------------------------------------------------
    let mut wifi = EspWifiDriver::new(peripherals.modem, sysloop, Some(nvs.clone()));
    let mut http_client = EspHttpClient::new();
    let mut saved_networks = NvsNetworkStore::new(nvs.clone());
    let mut credentials = NvsCredentialStore::new(nvs);
    let mut smtp_factory = EspSmtpStreamFactory::new();

    // --- A7682E 4G modem (off by default; powered on by `modem on`) ---------
    // Board pin map (from T-Deck-Pro utilities.h):
    //   POWER_EN = GPIO41, PWRKEY = GPIO40,
    //   modem RX = GPIO10 (ESP TX), modem TX = GPIO11 (ESP RX).
    // UART1 is free (UART0 is the USB console, the main ESP wifi/BT peripheral
    // used elsewhere is unrelated to the hardware UARTs).
    let uart_config = UartConfig::new().baudrate(Hertz(115_200));
    let modem_uart = UartDriver::new(
        peripherals.uart1,
        pins.gpio10,
        pins.gpio11,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        None::<esp_idf_svc::hal::gpio::AnyIOPin>,
        &uart_config,
    )
    .unwrap();
    let modem_pwrkey = PinDriver::output(pins.gpio40.downgrade_output()).unwrap();
    let modem_power_en = PinDriver::output(pins.gpio41.downgrade_output()).unwrap();
    let mut modem_driver = EspA7682EModem::new(modem_uart, modem_pwrkey, modem_power_en);

    // --- Init display + terminal + shell -----------------------------------------
    log::info!("Clearing display");
    epd.clear().unwrap();

    let mut term = Terminal::new("> ", TERM_COLS, TERM_ROWS);
    let mut shell = Shell::new();
    let boot = std::time::Instant::now();

    // --- Startup tasks ----------------------------------------------------------
    let boot_log = dynatac_core::startup::run_startup(&mut wifi, &mut saved_networks);
    for msg in &boot_log {
        term.push_output(msg);
    }

    // Initial render: draw the prompt + boot messages
    render_terminal(&mut epd, &term);
    flush(&mut epd);

    // --- Event loop --------------------------------------------------------------
    log::info!("Ready — type on the keyboard");
    loop {
        let mut needs_redraw = false;

        // Drain all buffered key events
        loop {
            match kb.poll() {
                Ok(Some(event)) => {
                    if shell.is_interactive() {
                        // Route keys to the active list selector
                        let mut ctx = ExecContext {
                            uptime_secs: boot.elapsed().as_secs(),
                            wifi: &mut wifi,
                            http: &mut http_client,
                            saved_networks: &mut saved_networks,
                            smtp: &mut smtp_factory,
                            credentials: &mut credentials,
                            modem: &mut modem_driver,
                        };
                        let was_interactive = true;
                        match shell.handle_interactive_key(event, &mut ctx) {
                            ShellAction::Output(output) => {
                                if !output.is_empty() {
                                    term.clear();
                                    term.push_output(&output);
                                }
                            }
                            ShellAction::Clear => {
                                term.clear();
                            }
                        }
                        // Exited list mode — restore input line
                        if was_interactive && !shell.is_interactive() {
                            term.set_show_input(true);
                        }
                        needs_redraw = true;
                    } else {
                        // Normal terminal input
                        match term.handle_key(event) {
                            TerminalAction::Redraw => {
                                needs_redraw = true;
                            }
                            TerminalAction::Execute(cmd) => {
                                let mut ctx = ExecContext {
                                    uptime_secs: boot.elapsed().as_secs(),
                                    wifi: &mut wifi,
                                    http: &mut http_client,
                                    saved_networks: &mut saved_networks,
                                    smtp: &mut smtp_factory,
                                    credentials: &mut credentials,
                                    modem: &mut modem_driver,
                                };
                                match shell.execute(&cmd, &mut ctx) {
                                    ShellAction::Output(output) => {
                                        // Entered list mode — hide input line
                                        if shell.is_interactive() {
                                            term.set_show_input(false);
                                            term.clear();
                                        }
                                        term.push_output(&output);
                                    }
                                    ShellAction::Clear => {
                                        term.clear();
                                    }
                                }
                                needs_redraw = true;
                            }
                            TerminalAction::None => {}
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::error!("keyboard poll error: {:?}", e);
                    break;
                }
            }
        }

        if needs_redraw {
            render_terminal(&mut epd, &term);
            flush(&mut epd);
        }

        sleep(Duration::from_millis(10));
    }
}

/// Render the full terminal state to the EPD framebuffer.
fn render_terminal(epd: &mut Epd, term: &Terminal) {
    for row in 0..TERM_ROWS {
        epd.clear_line((row as u16) * ROW_HEIGHT);
    }
    for cell in term.render() {
        let x = cell.col * 8;
        let y = cell.row as u16 * ROW_HEIGHT;
        epd.draw_char(x, y, cell.ch).unwrap();
    }
}

/// Flush dirty regions to the e-paper display.
fn flush(epd: &mut Epd) {
    if let Err(e) = epd.try_flush() {
        log::error!("display flush error: {:?}", e);
    }
}
