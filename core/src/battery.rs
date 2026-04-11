//! Pure fuel-gauge logic for the BQ27220 battery monitor.
//!
//! The device side owns the I2C transactions; this module defines the
//! [`BatteryDriver`] trait it implements, plus pure parsers for the
//! register payloads so they can be unit-tested on the host.
//!
//! Register layout confirmed against the T-Deck-Pro reference driver
//! (`T-Deck-Pro/lib/BQ27220/bq27220_def.h` + `bq27220.h`). All
//! multi-byte BQ27220 registers are little-endian on the wire.

/// Reported battery charging state, decoded from the BQ27220
/// `BatteryStatus` register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargeState {
    Charging,
    Discharging,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryError {
    /// Underlying I2C transaction failed.
    Bus,
}

/// Interface a hardware battery gauge must implement. Exposed to
/// programs via `ExecContext`.
pub trait BatteryDriver {
    /// State-of-charge as a percentage in 0..=100.
    fn level(&mut self) -> Result<u8, BatteryError>;

    /// Coarse charging state derived from the BatteryStatus register.
    fn charge_state(&mut self) -> Result<ChargeState, BatteryError>;
}

// --- BQ27220 register map -----------------------------------------------------

/// BQ27220 I2C 7-bit address.
pub const BQ27220_ADDR: u8 = 0x55;

/// BatteryStatus command — 2-byte read.
pub const REG_BATTERY_STATUS: u8 = 0x0A;

/// StateOfCharge command — 2-byte read, value is percent.
pub const REG_STATE_OF_CHARGE: u8 = 0x2C;

// --- Pure parsers -------------------------------------------------------------

/// Decode the raw (little-endian) StateOfCharge payload into 0..=100.
/// Values above 100 are clamped; over-reporting is benign and has been
/// observed briefly during state transitions.
pub fn parse_soc(raw: [u8; 2]) -> u8 {
    let value = u16::from_le_bytes(raw);
    if value > 100 {
        100
    } else {
        value as u8
    }
}

/// Decode BatteryStatus into a [`ChargeState`].
///
/// Bit 0 = DSG (discharging), bit 9 = FC (full charge). DSG takes
/// priority: if the gauge reports discharging, that's the ground truth
/// regardless of the FC flag.
pub fn parse_battery_status(raw: [u8; 2]) -> ChargeState {
    let bits = u16::from_le_bytes(raw);
    let dsg = bits & (1 << 0) != 0;
    let fc = bits & (1 << 9) != 0;
    if dsg {
        ChargeState::Discharging
    } else if fc {
        ChargeState::Full
    } else {
        ChargeState::Charging
    }
}

/// In-memory test double for [`BatteryDriver`]. Default state is a
/// healthy mid-charge battery; tests can override the fields directly.
pub struct MockBatteryDriver {
    pub level: Result<u8, BatteryError>,
    pub charge_state: Result<ChargeState, BatteryError>,
}

impl MockBatteryDriver {
    pub fn new() -> Self {
        Self {
            level: Ok(75),
            charge_state: Ok(ChargeState::Discharging),
        }
    }
}

impl Default for MockBatteryDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl BatteryDriver for MockBatteryDriver {
    fn level(&mut self) -> Result<u8, BatteryError> {
        self.level.clone()
    }

    fn charge_state(&mut self) -> Result<ChargeState, BatteryError> {
        self.charge_state.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soc_zero() {
        assert_eq!(parse_soc([0, 0]), 0);
    }

    #[test]
    fn soc_typical() {
        assert_eq!(parse_soc([87, 0]), 87);
    }

    #[test]
    fn soc_full() {
        assert_eq!(parse_soc([100, 0]), 100);
    }

    #[test]
    fn soc_clamped_above_100() {
        assert_eq!(parse_soc([105, 0]), 100);
    }

    #[test]
    fn soc_high_byte_clamped() {
        assert_eq!(parse_soc([0x00, 0x01]), 100);
    }

    #[test]
    fn battery_status_discharging() {
        assert_eq!(parse_battery_status([0x01, 0x00]), ChargeState::Discharging);
    }

    #[test]
    fn battery_status_charging() {
        assert_eq!(parse_battery_status([0x00, 0x00]), ChargeState::Charging);
    }

    #[test]
    fn battery_status_full() {
        // FC is bit 9 → high byte bit 1 → 0x02
        assert_eq!(parse_battery_status([0x00, 0x02]), ChargeState::Full);
    }

    #[test]
    fn battery_status_dsg_takes_priority_over_fc() {
        assert_eq!(parse_battery_status([0x01, 0x02]), ChargeState::Discharging);
    }

    #[test]
    fn battery_status_ignores_unrelated_bits() {
        // BATTPRES + AUTH_GD set, DSG/FC clear → still Charging
        assert_eq!(parse_battery_status([0b0001_1000, 0x00]), ChargeState::Charging);
    }
}
