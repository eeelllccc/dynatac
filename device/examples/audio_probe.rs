//! Audio hardware probe for the A7682E 4G version of T-Deck-Pro.
//!
//! Answers the key questions needed before implementing phone calls:
//!
//!   1. Is the A7682E speaker output wired? (TTS playback — already confirmed YES)
//!   2. Is the A7682E mic wired? (record then play, with explicit per-step results)
//!   3. What volume commands work? (COUTGAIN, CMIC)
//!   4. Does the ESP32 PDM mic (IO17/IO18) produce audio? (shown on a fresh screen)
//!
//! Each step is gated by a keypress. Check the serial monitor for full AT logs.
//!
//! Build:
//!   cargo build -p dynatac --example audio_probe
//! Flash:
//!   espflash flash target/xtensa-esp32s3-espidf/debug/examples/audio_probe --monitor

#[path = "../src/display.rs"]
mod display;
#[path = "../src/i2c_bus.rs"]
mod i2c_bus;
#[path = "../src/keyboard.rs"]
mod keyboard;
#[path = "../src/modem.rs"]
mod modem;

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::{
    gpio::{AnyOutputPin, IOPin, OutputPin, PinDriver},
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, Dma, SpiConfig, SpiDeviceDriver, SpiDriver},
    uart::{config::Config as UartConfig, UartDriver},
    units::Hertz,
};

use display::Epd;
use dynatac_core::font::FontSize;
use dynatac_core::modem::Modem;
use i2c_bus::I2cBus;
use keyboard::Keyboard;
use modem::EspA7682EModem;

const KEYBOARD_ADDR: u8 = 0x34;
const FONT: FontSize = FontSize::Medium;
const ROW_HEIGHT: u16 = FONT.height();

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Booting audio_probe");

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    // --- Deselect SPI devices before bus init ------------------------------------
    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    {
        let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
        sd_cs.set_high().unwrap();
    }

    // --- SPI bus -----------------------------------------------------------------
    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        pins.gpio36,
        pins.gpio33,
        Some(pins.gpio47),
        &DriverConfig::new().dma(Dma::Auto(4096)),
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

    // --- I2C bus (keyboard) ------------------------------------------------------
    let i2c_driver = I2cDriver::new(
        peripherals.i2c0,
        pins.gpio13,
        pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    )
    .unwrap();
    let i2c_bus = I2cBus::new(i2c_driver);
    let mut kb = Keyboard::new(i2c_bus.device(KEYBOARD_ADDR)).unwrap();

    // --- Modem -------------------------------------------------------------------
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
    let mut modem = EspA7682EModem::new(modem_uart, modem_pwrkey, modem_power_en);

    // --- Init display ------------------------------------------------------------
    epd.clear().unwrap();
    let mut y: u16 = 0;

    print_line(&mut epd, "=== Audio Probe 2 ===", &mut y);
    print_line(&mut epd, "Tests A7682E mic +", &mut y);
    print_line(&mut epd, "volume + PDM mic.", &mut y);
    print_line(&mut epd, "Check serial log too.", &mut y);
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Press key to power", &mut y);
    print_line(&mut epd, "on modem...", &mut y);
    wait_key(&mut kb);

    // --- Power on modem ----------------------------------------------------------
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "Powering on modem...", &mut y);
    match modem.power_on() {
        Ok(()) => {
            log::info!("modem power on OK");
            print_line(&mut epd, "[OK] Modem up.", &mut y);
        }
        Err(e) => {
            log::error!("modem power on failed: {:?}", e);
            print_line(&mut epd, "[ERR] Modem failed.", &mut y);
            loop { sleep(Duration::from_secs(1)); }
        }
    }

    // --- TEST 1: Volume commands -------------------------------------------------
    // Find out what volume/gain commands the A7682E accepts.
    // AT+COUTGAIN sets playback volume for CCMXPLAY (separate from TTS volume).
    // AT+CMIC sets microphone gain.
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "TEST 1: Volume cmds", &mut y);
    print_line(&mut epd, "Press key to start.", &mut y);
    wait_key(&mut kb);

    // Try setting output gain (0-4) to maximum before any playback
    let coutgain_set = modem.send_raw("AT+COUTGAIN=0,4");
    log::info!("AT+COUTGAIN=0,4: {:?}", coutgain_set);
    let coutgain_q = modem.send_raw("AT+COUTGAIN?");
    log::info!("AT+COUTGAIN?: {:?}", coutgain_q);

    // Mic gain (channel 0, gain 10 = +20dB)
    let cmic_set = modem.send_raw("AT+CMIC=0,10");
    log::info!("AT+CMIC=0,10: {:?}", cmic_set);
    let cmic_q = modem.send_raw("AT+CMIC?");
    log::info!("AT+CMIC?: {:?}", cmic_q);

    // Also try CLVL (works on some firmware variants when in a call)
    let clvl_q = modem.send_raw("AT+CLVL?");
    log::info!("AT+CLVL?: {:?}", clvl_q);

    show_at_result(&mut epd, "COUTGAIN", &coutgain_set, &mut y);
    show_at_result(&mut epd, "CMIC", &cmic_set, &mut y);
    show_at_result(&mut epd, "CLVL?", &clvl_q, &mut y);

    print_line(&mut epd, "Press key to continue.", &mut y);
    wait_key(&mut kb);

    // --- TEST 2: Record then play back -------------------------------------------
    // Shows a separate on-screen result for each step so we know exactly
    // which part fails.
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "TEST 2: Mic record", &mut y);
    print_line(&mut epd, "Speak 3s after key.", &mut y);
    wait_key(&mut kb);

    print_line(&mut epd, "Recording 3s...", &mut y);
    let rec_start = modem.send_raw(r#"AT+CREC=1,"C:/test.wav""#);
    log::info!("AT+CREC=1: {:?}", rec_start);
    show_at_result(&mut epd, "CREC start", &rec_start, &mut y);

    sleep(Duration::from_secs(3));

    let rec_stop = modem.send_raw("AT+CREC=0");
    log::info!("AT+CREC=0: {:?}", rec_stop);
    show_at_result(&mut epd, "CREC stop", &rec_stop, &mut y);

    if rec_start.is_ok() && rec_stop.is_ok() {
        // Check if file exists on modem filesystem
        let ls = modem.send_raw("AT+FSLS=C:/");
        log::info!("AT+FSLS=C:/: {:?}", ls);

        print_line(&mut epd, "Playing back...", &mut y);
        let play = modem.send_raw(r#"AT+CCMXPLAY="C:/test.wav",0,1"#);
        log::info!("AT+CCMXPLAY: {:?}", play);
        show_at_result(&mut epd, "CCMXPLAY", &play, &mut y);

        if play.is_ok() {
            // Wait for +AUDIOSTATE: audio play stop URC
            sleep(Duration::from_secs(5));
            print_line(&mut epd, "Hear your voice?", &mut y);
            print_line(&mut epd, "(silent=mic unwired)", &mut y);
        }
    }

    print_line(&mut epd, "Press key to continue.", &mut y);
    wait_key(&mut kb);

    // --- TEST 3: TTS confirm at higher volume ------------------------------------
    // Replay TTS after setting COUTGAIN to confirm volume commands took effect.
    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "TEST 3: TTS recheck", &mut y);
    print_line(&mut epd, "Plays hello again.", &mut y);
    print_line(&mut epd, "Press key to start.", &mut y);
    wait_key(&mut kb);

    let _ = modem.send_raw("AT+CTTSPARAM=2,3,0,1,1");
    let tts = modem.send_raw(r#"AT+CTTS=2,"Hello World""#);
    log::info!("AT+CTTS: {:?}", tts);
    show_at_result(&mut epd, "CTTS", &tts, &mut y);
    sleep(Duration::from_secs(5));

    print_line(&mut epd, "Press key to continue.", &mut y);
    wait_key(&mut kb);

    // --- TEST 4: ESP32-side PDM microphone ---------------------------------------
    // Fresh screen so results are visible.
    epd.clear().unwrap();
    y = 0;

    print_line(&mut epd, "TEST 4: PDM mic", &mut y);
    print_line(&mut epd, "IO17=DATA IO18=CLK", &mut y);
    print_line(&mut epd, "Press key to test.", &mut y);
    wait_key(&mut kb);

    match test_pdm_mic() {
        PdmResult::GotAudio { max_abs } => {
            log::info!("PDM mic: got audio max_abs={}", max_abs);
            print_line(&mut epd, "[OK] Got audio!", &mut y);
            print_line(&mut epd, &format!("  max_abs={}", max_abs), &mut y);
            print_line(&mut epd, "PDM mic is live.", &mut y);
        }
        PdmResult::AllZero => {
            log::info!("PDM mic: all zeros");
            print_line(&mut epd, "[WARN] All zeros.", &mut y);
            print_line(&mut epd, "Mic absent or wrong", &mut y);
            print_line(&mut epd, "pin assignment.", &mut y);
        }
        PdmResult::SetupError(ref msg) => {
            log::info!("PDM mic setup error: {}", msg);
            print_line(&mut epd, "[ERR] I2S setup:", &mut y);
            let s: String = msg.chars().take(28).collect();
            print_line(&mut epd, &s, &mut y);
        }
    }

    print_line(&mut epd, "", &mut y);
    print_line(&mut epd, "=== Done ===", &mut y);
    print_line(&mut epd, "Check serial log for", &mut y);
    print_line(&mut epd, "full AT responses.", &mut y);
    print_line(&mut epd, "Press key to exit.", &mut y);
    wait_key(&mut kb);

    epd.power_down().unwrap();
}

// ---------------------------------------------------------------------------
// PDM microphone test
// ---------------------------------------------------------------------------

enum PdmResult {
    GotAudio { max_abs: i32 },
    AllZero,
    SetupError(String),
}

fn test_pdm_mic() -> PdmResult {
    use esp_idf_svc::sys::*;

    const SAMPLE_RATE: u32 = 16_000;
    const NUM_SAMPLES: usize = 1024;
    const BUF_BYTES: usize = NUM_SAMPLES * 2; // 16-bit samples

    unsafe {
        let chan_cfg = i2s_chan_config_t {
            id: i2s_port_t_I2S_NUM_0,
            role: i2s_role_t_I2S_ROLE_MASTER,
            dma_desc_num: 4,
            dma_frame_num: 256,
            auto_clear: false,
            intr_priority: 0,
        };

        let mut rx_handle: i2s_chan_handle_t = core::ptr::null_mut();
        let ret = i2s_new_channel(&chan_cfg, core::ptr::null_mut(), &mut rx_handle);
        if ret != ESP_OK {
            return PdmResult::SetupError(format!("i2s_new_channel: {}", ret));
        }

        // gpio_cfg uses a union: single mic uses the `din` variant.
        let mut gpio_cfg = i2s_pdm_rx_gpio_config_t::default();
        gpio_cfg.clk = 18; // IO18
        gpio_cfg.__bindgen_anon_1.din = 17; // IO17

        let pdm_rx_cfg = i2s_pdm_rx_config_t {
            clk_cfg: i2s_pdm_rx_clk_config_t {
                sample_rate_hz: SAMPLE_RATE,
                clk_src: soc_periph_i2s_clk_src_t_I2S_CLK_SRC_DEFAULT,
                mclk_multiple: i2s_mclk_multiple_t_I2S_MCLK_MULTIPLE_256,
                dn_sample_mode: i2s_pdm_dsr_t_I2S_PDM_DSR_8S,
                bclk_div: 8,
            },
            slot_cfg: i2s_pdm_rx_slot_config_t {
                data_bit_width: i2s_data_bit_width_t_I2S_DATA_BIT_WIDTH_16BIT,
                slot_bit_width: i2s_slot_bit_width_t_I2S_SLOT_BIT_WIDTH_AUTO,
                slot_mode: i2s_slot_mode_t_I2S_SLOT_MODE_MONO,
                slot_mask: i2s_pdm_slot_mask_t_I2S_PDM_SLOT_LEFT,
            },
            gpio_cfg,
        };

        let ret = i2s_channel_init_pdm_rx_mode(rx_handle, &pdm_rx_cfg);
        if ret != ESP_OK {
            i2s_del_channel(rx_handle);
            return PdmResult::SetupError(format!("init_pdm_rx: {}", ret));
        }

        let ret = i2s_channel_enable(rx_handle);
        if ret != ESP_OK {
            i2s_del_channel(rx_handle);
            return PdmResult::SetupError(format!("enable: {}", ret));
        }

        let mut buf = vec![0u8; BUF_BYTES];
        let mut bytes_read: usize = 0;
        let ret = i2s_channel_read(
            rx_handle,
            buf.as_mut_ptr() as *mut _,
            BUF_BYTES,
            &mut bytes_read,
            500,
        );

        i2s_channel_disable(rx_handle);
        i2s_del_channel(rx_handle);

        if ret != ESP_OK && bytes_read == 0 {
            return PdmResult::SetupError(format!("i2s_read: {}", ret));
        }

        let samples: &[i16] = core::slice::from_raw_parts(
            buf.as_ptr() as *const i16,
            bytes_read / 2,
        );
        let max_abs = samples.iter().map(|&s| s.unsigned_abs() as i32).max().unwrap_or(0);
        log::info!("PDM mic: {} bytes read, max_abs={}", bytes_read, max_abs);

        if max_abs > 100 {
            PdmResult::GotAudio { max_abs }
        } else {
            PdmResult::AllZero
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Print a one-line OK/ERR summary for an AT result.
fn show_at_result(
    epd: &mut Epd,
    name: &str,
    result: &Result<Vec<String>, dynatac_core::modem::ModemError>,
    y: &mut u16,
) {
    match result {
        Ok(_) => {
            let s = format!("  [OK] {}", name);
            print_line(epd, &s, y);
        }
        Err(e) => {
            let s = format!("  [ERR] {}: {:?}", name, e);
            print_line(epd, &s, y);
        }
    }
}

fn print_line(epd: &mut Epd, text: &str, y: &mut u16) {
    if *y >= display::HEIGHT {
        return;
    }
    epd.clear_line(*y, ROW_HEIGHT);
    epd.draw_text(0, *y, FONT.font(), text);
    if let Err(e) = epd.try_flush() {
        log::error!("display flush: {:?}", e);
    }
    *y += ROW_HEIGHT;
}

fn wait_key(kb: &mut Keyboard) {
    loop {
        match kb.poll() {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(e) => log::warn!("kb poll: {:?}", e),
        }
        sleep(Duration::from_millis(10));
    }
}
