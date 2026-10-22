// The SPI2 bus as a `dynatac_core::spi::wire::Wire`: raw pins and bytes,
// nothing else. All bus rules (chip selects, release bytes, bring-up order)
// live in core (`spi::bus`), tested against `MockWire`.
//
// Hardware (docs/spi-research.md §1):
//   SCK GPIO36, MOSI GPIO33, MISO GPIO47
//   e-paper CS 34, DC 35, BUSY 37 · SD CS 48 · LoRa CS 3, RST 4, BUSY 6,
//   DIO1 5, supply enable 46
//
// Built on ESP-IDF `spi_master` with one `spi_device` per clock speed, all
// with CS = -1: chip selects are plain GPIOs driven by core through `set`.
// Only the spi task uses this, so `spi_master`'s one weakness (use from
// several tasks) can't arise.
//
// Callee invariants (the `Wire` contract):
//   - `transfer` clocks exactly `out.len()` bytes, mode 0, at the nearest
//     speed the hardware can make at or below `clock`; it touches no pin.
//   - Outputs start low, LoRa supply off; no clock goes out before core's
//     bring-up powers LoRa (B5: an unpowered SX1262 blocks the bus).
//
// Caller invariants:
//   - One `EspWire`; used from one task.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use esp_idf_svc::hal::delay::Ets;
use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Gpio3, Gpio33, Gpio34, Gpio35, Gpio36, Gpio37, Gpio4, Gpio46, Gpio47, Gpio48, Gpio5, Gpio6, IOPin, Input as InputMode, Output as OutputMode, OutputPin, PinDriver};
use esp_idf_svc::hal::spi::{config::{Config, DriverConfig}, Dma, SpiDeviceDriver, SpiDriver, SPI2};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys::{spi_device_get_actual_freq, EspError};

use dynatac_core::spi::wire::{Clock, Hz, Input, Output, Wire, WireError};
use dynatac_core::spi::Millis;

/// DMA buffer size: ≥ the largest transaction (e-paper chunks of 4092).
const DMA_BYTES: usize = 4096;

pub struct Pins {
    pub sck: Gpio36,
    pub mosi: Gpio33,
    pub miso: Gpio47,
    pub epd_cs: Gpio34,
    pub epd_dc: Gpio35,
    pub epd_busy: Gpio37,
    pub sd_cs: Gpio48,
    pub lora_cs: Gpio3,
    pub lora_rst: Gpio4,
    pub lora_busy: Gpio6,
    pub lora_dio1: Gpio5,
    pub lora_en: Gpio46,
}

type Out = PinDriver<'static, AnyOutputPin, OutputMode>;
pub type In = PinDriver<'static, AnyIOPin, InputMode>;

pub struct EspWire {
    driver: Arc<SpiDriver<'static>>,
    /// One device per clock speed asked for, created on first use.
    devices: HashMap<u32, SpiDeviceDriver<'static, Arc<SpiDriver<'static>>>>,
    outputs: HashMap<Output, Out>,
    inputs: HashMap<Input, In>,
    epoch: Instant,
}

impl EspWire {
    pub fn new(spi: SPI2, p: Pins) -> Result<Self, EspError> {
        let driver = SpiDriver::new(spi, p.sck, p.mosi, Some(p.miso), &DriverConfig::new().dma(Dma::Auto(DMA_BYTES)))?;
        let out = |pin: AnyOutputPin| -> Result<Out, EspError> {
            let mut d = PinDriver::output(pin)?;
            d.set_low()?;
            Ok(d)
        };
        let outputs = HashMap::from([
            (Output::LoraEn, out(p.lora_en.downgrade_output())?),
            (Output::LoraCs, out(p.lora_cs.downgrade_output())?),
            (Output::LoraRst, out(p.lora_rst.downgrade_output())?),
            (Output::EpdCs, out(p.epd_cs.downgrade_output())?),
            (Output::EpdDc, out(p.epd_dc.downgrade_output())?),
            (Output::SdCs, out(p.sd_cs.downgrade_output())?),
        ]);
        let inputs = HashMap::from([
            (Input::EpdBusy, PinDriver::input(p.epd_busy.downgrade())?),
            (Input::LoraBusy, PinDriver::input(p.lora_busy.downgrade())?),
            (Input::LoraDio1, PinDriver::input(p.lora_dio1.downgrade())?),
        ]);
        Ok(EspWire { driver: Arc::new(driver), devices: HashMap::new(), outputs, inputs, epoch: Instant::now() })
    }

    /// The input pin drivers, for the spi task to arm interrupts on.
    pub fn input_mut(&mut self, pin: Input) -> &mut In {
        self.inputs.get_mut(&pin).expect("every input is configured")
    }

    fn device(&mut self, clock: Hz) -> Result<&mut SpiDeviceDriver<'static, Arc<SpiDriver<'static>>>, WireError> {
        if !self.devices.contains_key(&clock.0) {
            let config = Config::new().baudrate(Hertz(clock.0));
            let dev = SpiDeviceDriver::new(self.driver.clone(), None::<AnyOutputPin>, &config).map_err(err)?;
            let mut khz = 0;
            // Log what the hardware actually makes of the request.
            unsafe { spi_device_get_actual_freq(dev.device(), &mut khz) };
            log::info!("spi: device for {} Hz runs at {} kHz", clock.0, khz);
            self.devices.insert(clock.0, dev);
        }
        Ok(self.devices.get_mut(&clock.0).expect("just inserted"))
    }
}

fn err(e: EspError) -> WireError {
    WireError(format!("{e:?}"))
}

impl Wire for EspWire {
    fn set(&mut self, pin: Output, high: bool) {
        let d = self.outputs.get_mut(&pin).expect("every output is configured");
        if let Err(e) = if high { d.set_high() } else { d.set_low() } {
            log::error!("spi: setting {pin:?} failed: {e:?}");
        }
    }

    fn get(&mut self, pin: Input) -> bool {
        self.inputs[&pin].is_high()
    }

    fn transfer(&mut self, clock: Hz, out: &[u8], input: Option<&mut [u8]>) -> Result<(), WireError> {
        let dev = self.device(clock)?;
        match input {
            Some(buf) => dev.transfer(buf, out).map_err(err),
            None => dev.write(out).map_err(err),
        }
    }

    fn pause_us(&mut self, us: u32) {
        Ets::delay_us(us);
    }
}

impl Clock for EspWire {
    fn now(&self) -> Millis {
        self.epoch.elapsed().as_millis() as Millis
    }
}
