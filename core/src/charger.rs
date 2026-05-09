// Pure register-byte helpers for the BQ25896 charger / PMIC.
//
// The BQ25896 lives on the shared I2C bus at 0x6B. This module owns the
// register layout and the bit-twiddling for the two operations we need:
//
//   1. Disable the long-press QON reset, so accidentally holding the
//      power button while running no longer reboots the device.
//   2. Enter ship mode (BATFET disabled), the cleanest "power off"
//      available on this hardware. Wake by holding QON or plugging USB.
//
// All functions here are pure: they take the current register byte and
// return the new byte with one bit flipped. The hardware wrapper in
// `device/src/charger.rs` does the I2C read-modify-write.

/// I2C address of the BQ25896 on the T-Deck-Pro.
pub const BQ25896_ADDR: u8 = 0x6B;

/// REG07: Charge Termination / Timer Control.
///   bit 7 EN_TERM
///   bit 6 STAT_DIS
///   bits 5:4 WATCHDOG (00=disable, 01=40s, 10=80s, 11=160s; default 01)
///   bit 3 EN_TIMER
///   bits 2:0 CHG_TIMER
pub const REG07: u8 = 0x07;

const WATCHDOG_MASK: u8 = 0b0011_0000;

/// REG09: System Function Setting (BQ25896 datasheet, Table 11).
///   bit 7 FORCE_ICO
///   bit 6 TMR2X_EN
///   bit 5 BATFET_DIS    (1 = BATFET off → ship mode)
///   bit 4 JEITA_VSET
///   bit 3 BATFET_DLY
///   bit 2 BATFET_RST_EN (1 = long QON press triggers full system reset; default)
///   bit 1 PUMPX_UP
///   bit 0 PUMPX_DN
pub const REG09: u8 = 0x09;

const BATFET_DIS_MASK: u8 = 1 << 5;
const BATFET_RST_EN_MASK: u8 = 1 << 2;

/// Clear REG07 bits 5:4 to disable the I2C watchdog. Without this the
/// BQ25896 reverts every register (including our REG09 customisations)
/// back to defaults whenever ~40 s elapses without an I2C transaction
/// — silently re-enabling `BATFET_RST_EN` and bringing the long-press
/// QON reset back from the dead.
pub fn disable_watchdog(reg07: u8) -> u8 {
    reg07 & !WATCHDOG_MASK
}

/// Clear BATFET_RST_EN so a long QON press is no longer treated as a
/// system reset request by the PMIC.
pub fn disable_long_press_reset(reg09: u8) -> u8 {
    reg09 & !BATFET_RST_EN_MASK
}

/// Set BATFET_DIS to enter ship mode. The system loses power once the
/// (small, hardware-fixed) `tSHIPMODE` delay elapses. Wake by holding
/// the QON button or plugging in USB.
pub fn enter_ship_mode(reg09: u8) -> u8 {
    reg09 | BATFET_DIS_MASK
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargerError {
    /// Underlying I2C transaction failed.
    Bus,
}

/// Interface a hardware PMIC driver must implement. Exposed to programs
/// via `ExecContext` so the `power` program can request shutdown.
pub trait ChargerDriver {
    /// Enter ship mode (BATFET_DIS=1). The device will lose power
    /// shortly after this call returns. Has no effect when USB is
    /// connected — the BQ25896 silently ignores shutdown requests
    /// while VBUS is present.
    fn shutdown(&mut self) -> Result<(), ChargerError>;
}

/// In-memory test double for [`ChargerDriver`]. Records whether
/// shutdown was called.
pub struct MockChargerDriver {
    pub shutdown_result: Result<(), ChargerError>,
    pub shutdown_called: bool,
}

impl MockChargerDriver {
    pub fn new() -> Self {
        Self {
            shutdown_result: Ok(()),
            shutdown_called: false,
        }
    }
}

impl Default for MockChargerDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl ChargerDriver for MockChargerDriver {
    fn shutdown(&mut self) -> Result<(), ChargerError> {
        self.shutdown_called = true;
        self.shutdown_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disable_reset_clears_bit_2() {
        // bit 2 (BATFET_RST_EN) set, others zero.
        assert_eq!(disable_long_press_reset(0b0000_0100), 0b0000_0000);
    }

    #[test]
    fn disable_reset_is_idempotent() {
        assert_eq!(disable_long_press_reset(0b0000_0000), 0b0000_0000);
    }

    #[test]
    fn ship_mode_sets_bit_5() {
        // Setting bit 5 (BATFET_DIS) → ship mode.
        assert_eq!(enter_ship_mode(0b0000_0000), 0b0010_0000);
    }

    #[test]
    fn ship_mode_is_idempotent() {
        assert_eq!(enter_ship_mode(0b0010_0000), 0b0010_0000);
    }

    #[test]
    fn disable_reset_preserves_other_bits() {
        // Every bit except bit 2 set.
        let before: u8 = 0b1111_1011;
        let after = disable_long_press_reset(before);
        assert_eq!(after, 0b1111_1011); // bit 2 was already 0; nothing else moved
        // Now flip bit 2 on and try again.
        let before = 0b1111_1111;
        let after = disable_long_press_reset(before);
        assert_eq!(after, 0b1111_1011);
    }

    #[test]
    fn ship_mode_preserves_other_bits() {
        // Every bit except bit 5 set.
        let before: u8 = 0b1101_1111;
        let after = enter_ship_mode(before);
        assert_eq!(after, 0b1111_1111);
    }

    #[test]
    fn disable_watchdog_clears_bits_5_and_4() {
        // Default reg value 0b0001_1010 (WD=01 = 40s, EN_TIMER=1, CHG_TIMER=010).
        let before = 0b0001_1010;
        let after = disable_watchdog(before);
        assert_eq!(after, 0b0000_1010);
    }

    #[test]
    fn disable_watchdog_handles_max_value() {
        // WD=11 (160s) → both bits should clear.
        assert_eq!(disable_watchdog(0b0011_0000), 0b0000_0000);
    }

    #[test]
    fn disable_watchdog_preserves_other_bits() {
        // Every bit set; only bits 5:4 should clear.
        assert_eq!(disable_watchdog(0xFF), 0b1100_1111);
    }

    #[test]
    fn disable_watchdog_is_idempotent() {
        assert_eq!(disable_watchdog(0b0000_0000), 0b0000_0000);
    }

    #[test]
    fn helpers_act_on_independent_bits() {
        // BATFET_DIS (bit 5) and BATFET_RST_EN (bit 2) are independent —
        // toggling one must not disturb the other.
        let reg = 0b0000_0100; // BATFET_RST_EN set, BATFET_DIS clear
        let reg = disable_long_press_reset(reg);
        assert_eq!(reg, 0b0000_0000);
        let reg = enter_ship_mode(reg);
        assert_eq!(reg, 0b0010_0000); // BATFET_DIS now set
    }
}
