// Thin I2C wrapper for the BQ27220 fuel gauge on the T-Deck-Pro.
//
// All parsing and the `BatteryDriver` trait live in
// `dynatac-core::battery`; this file is just the transport: each call
// issues one `write_read` on the shared I2C bus and hands the raw
// bytes to the pure parser.
//
// Caller invariants:
//   - The provided `I2cDevice` must be bound to the BQ27220 address
//     (`dynatac_core::battery::BQ27220_ADDR` == 0x55) on the shared
//     bus shared with the keyboard, touch, gyro, and charger.
//
// Callee invariants:
//   - `new()` issues one throwaway `write_read` to wake the chip. The
//     BQ27220 sleeps to save power and its first I2C transaction after
//     power-up reliably times out while it wakes — we eat that failure
//     here so user-visible commands always hit a responsive chip.

use dynatac_core::battery::{
    parse_battery_status, parse_soc, BatteryDriver, BatteryError, ChargeState,
    REG_BATTERY_STATUS, REG_STATE_OF_CHARGE,
};

use crate::i2c_bus::I2cDevice;

/// Device-side driver for the BQ27220 fuel gauge.
pub struct Battery<'a, 'd> {
    dev: I2cDevice<'a, 'd>,
}

impl<'a, 'd> Battery<'a, 'd> {
    pub fn new(dev: I2cDevice<'a, 'd>) -> Self {
        let battery = Self { dev };
        // Wake the chip. This first transaction is expected to time out
        // on a cold boot; we deliberately discard the result.
        let mut buf = [0u8; 2];
        let _ = battery.dev.write_read(&[REG_BATTERY_STATUS], &mut buf);
        battery
    }

    fn read_u16(&self, reg: u8) -> Result<[u8; 2], BatteryError> {
        let mut buf = [0u8; 2];
        self.dev
            .write_read(&[reg], &mut buf)
            .map_err(|e| {
                log::warn!("battery I2C read (reg 0x{:02X}) failed: {:?}", reg, e);
                BatteryError::Bus
            })?;
        Ok(buf)
    }
}

impl<'a, 'd> BatteryDriver for Battery<'a, 'd> {
    fn level(&mut self) -> Result<u8, BatteryError> {
        let raw = self.read_u16(REG_STATE_OF_CHARGE)?;
        Ok(parse_soc(raw))
    }

    fn charge_state(&mut self) -> Result<ChargeState, BatteryError> {
        let raw = self.read_u16(REG_BATTERY_STATUS)?;
        Ok(parse_battery_status(raw))
    }
}
