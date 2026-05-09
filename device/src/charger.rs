// Thin I2C wrapper for the BQ25896 PMIC on the T-Deck-Pro.
//
// All bit-twiddling lives in `dynatac_core::charger`; this file is
// just the transport. We do read-modify-write on REG09 to flip two
// independent bits:
//
//   - BATFET_RST_EN (cleared once at boot via `disable_long_press_reset`)
//     so a long press of the QON button no longer reboots the device.
//   - BATFET_DIS    (set on demand via `shutdown`) to enter ship mode.
//
// Caller invariants:
//   - The provided `I2cDevice` must be bound to the BQ25896 address
//     (`dynatac_core::charger::BQ25896_ADDR` == 0x6B) on the shared
//     I2C bus.
//   - `shutdown` is best-effort: the BQ25896 silently ignores the
//     request whenever USB is plugged in (it cannot turn the system
//     off while VBUS is present), so the call may complete normally
//     and the device may stay on.

use dynatac_core::charger::{
    disable_long_press_reset, disable_watchdog, enter_ship_mode, ChargerDriver, ChargerError,
    REG07, REG09,
};
use esp_idf_svc::hal::sys::EspError;

use crate::i2c_bus::I2cDevice;

/// Device-side driver for the BQ25896 charger / PMIC.
pub struct Charger<'a, 'd> {
    dev: I2cDevice<'a, 'd>,
}

impl<'a, 'd> Charger<'a, 'd> {
    pub fn new(dev: I2cDevice<'a, 'd>) -> Self {
        let charger = Self { dev };
        // Wake the chip — same pattern as the BQ27220 fuel gauge:
        // the first I2C transaction after cold boot may time out.
        let mut buf = [0u8; 1];
        let _ = charger.dev.write_read(&[REG09], &mut buf);
        charger
    }

    /// Clear BATFET_RST_EN so a long press of the QON button no longer
    /// triggers a system reset. Idempotent.
    pub fn disable_long_press_reset(&mut self) -> Result<(), EspError> {
        let before = self.read_reg(REG09)?;
        let target = disable_long_press_reset(before);
        if target != before {
            self.write_reg(REG09, target)?;
        }
        let after = self.read_reg(REG09)?;
        log::info!(
            "BQ25896 REG09: before=0b{:08b} target=0b{:08b} after=0b{:08b}",
            before, target, after
        );
        if after != target {
            log::warn!("BQ25896 REG09 readback mismatch — write may not have taken effect");
        }
        Ok(())
    }

    /// Disable the BQ25896 I2C watchdog. With the watchdog enabled
    /// (its default state, ~40 s timer) the chip silently reverts
    /// every register to defaults whenever the host stops talking to
    /// it — undoing any customisations we made (notably the
    /// `BATFET_RST_EN` clear). Disable it once at boot and we never
    /// have to touch it again.
    pub fn disable_watchdog(&mut self) -> Result<(), EspError> {
        let before = self.read_reg(REG07)?;
        let target = disable_watchdog(before);
        if target != before {
            self.write_reg(REG07, target)?;
        }
        let after = self.read_reg(REG07)?;
        log::info!(
            "BQ25896 REG07: before=0b{:08b} target=0b{:08b} after=0b{:08b}",
            before, target, after
        );
        if after != target {
            log::warn!("BQ25896 REG07 readback mismatch — write may not have taken effect");
        }
        Ok(())
    }

    fn read_reg(&self, reg: u8) -> Result<u8, EspError> {
        let mut buf = [0u8; 1];
        self.dev.write_read(&[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&self, reg: u8, val: u8) -> Result<(), EspError> {
        self.dev.write(&[reg, val])
    }
}

impl<'a, 'd> ChargerDriver for Charger<'a, 'd> {
    fn shutdown(&mut self) -> Result<(), ChargerError> {
        let current = self.read_reg(REG09).map_err(|e| {
            log::warn!("charger I2C read REG09 failed: {:?}", e);
            ChargerError::Bus
        })?;
        let next = enter_ship_mode(current);
        self.write_reg(REG09, next).map_err(|e| {
            log::warn!("charger I2C write REG09 failed: {:?}", e);
            ChargerError::Bus
        })?;
        Ok(())
    }
}
