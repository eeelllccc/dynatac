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
    /// Gauge did not reach an expected state within the timeout.
    Timeout,
}

/// Snapshot of BQ27220 diagnostic registers, returned by [`BatteryDriver::diag`].
#[derive(Debug, Clone)]
pub struct BatteryDiag {
    /// INITCOMP: gauge initialization complete. False means SOC is unreliable.
    pub initcomp: bool,
    /// CFGUPDATE: gauge is currently in config-update mode.
    pub cfg_update: bool,
    /// DesignCapacity register (mAh) — what the gauge thinks the battery holds.
    pub design_capacity_mah: u16,
    /// FullChargeCapacity register (mAh) — learned full-charge capacity.
    pub full_charge_capacity_mah: u16,
    /// RemainingCapacity register (mAh).
    pub remaining_capacity_mah: u16,
    /// Voltage register (mV).
    pub voltage_mv: u16,
    /// Current register (mA, signed: positive = charging, negative = discharging).
    pub current_ma: i16,
}

/// Interface a hardware battery gauge must implement. Exposed to
/// programs via `ExecContext`.
pub trait BatteryDriver {
    /// State-of-charge as a percentage in 0..=100.
    fn level(&mut self) -> Result<u8, BatteryError>;

    /// Coarse charging state derived from the BatteryStatus register.
    fn charge_state(&mut self) -> Result<ChargeState, BatteryError>;

    /// Read a diagnostic snapshot from several registers at once.
    fn diag(&mut self) -> Result<BatteryDiag, BatteryError>;

    /// Write the CEDV profile to the gauge's non-volatile Data Memory.
    /// One-time operation; persists across power cycles. Takes ~4 seconds.
    fn provision(&mut self) -> Result<(), BatteryError>;
}

// --- BQ27220 register map -----------------------------------------------------

/// BQ27220 I2C 7-bit address.
pub const BQ27220_ADDR: u8 = 0x55;

/// Control command — write 2-byte sub-command to trigger gauge operations.
pub const REG_CONTROL: u8 = 0x00;

/// Voltage command — 2-byte read, value in mV.
pub const REG_VOLTAGE: u8 = 0x08;

/// BatteryStatus command — 2-byte read.
pub const REG_BATTERY_STATUS: u8 = 0x0A;

/// Current command — 2-byte read, signed value in mA.
pub const REG_CURRENT: u8 = 0x0C;

/// RemainingCapacity command — 2-byte read, value in mAh.
pub const REG_REMAINING_CAPACITY: u8 = 0x10;

/// FullChargeCapacity command — 2-byte read, value in mAh.
pub const REG_FULL_CHARGE_CAPACITY: u8 = 0x12;

/// StateOfCharge command — 2-byte read, value is percent.
pub const REG_STATE_OF_CHARGE: u8 = 0x2C;

/// OperationStatus command — 2-byte read.
/// Bit 5 = INITCOMP (initialization complete), bit 10 = CFGUPDATE.
pub const REG_OPERATION_STATUS: u8 = 0x3A;

/// DesignCapacity command — 2-byte read, value in mAh.
pub const REG_DESIGN_CAPACITY: u8 = 0x3C;

/// MAC subclass address register — write [addr_lo, addr_hi, data...] here
/// to address a Data Memory location for reading or writing.
pub const REG_SELECT_SUBCLASS: u8 = 0x3E;

/// MAC data checksum register — write [checksum, length] here to commit a DM write.
pub const REG_MAC_DATA_SUM: u8 = 0x60;

// Control sub-commands (written as little-endian u16 to REG_CONTROL).
pub const CTRL_UNSEAL_KEY1: u16 = 0x0414;
pub const CTRL_UNSEAL_KEY2: u16 = 0x3672;
pub const CTRL_FULL_ACCESS_KEY: u16 = 0xFFFF;
pub const CTRL_RESET: u16 = 0x0041;
pub const CTRL_ENTER_CFG_UPDATE: u16 = 0x0090;
pub const CTRL_EXIT_CFG_UPDATE_REINIT: u16 = 0x0091;
pub const CTRL_SEALED: u16 = 0x0030;

// --- CEDV profile ---------------------------------------------------------------

/// One Data Memory parameter to write during provisioning.
#[derive(Debug, Clone, Copy)]
pub struct DmEntry {
    pub address: u16,
    pub data: DmData,
}

#[derive(Debug, Clone, Copy)]
pub enum DmData {
    U8(u8),
    U16(u16),
    I16(i16),
}

impl DmData {
    /// Big-endian byte representation and byte count for the DM write.
    pub fn be_bytes(self) -> ([u8; 2], usize) {
        match self {
            DmData::U8(v) => ([v, 0], 1),
            DmData::U16(v) => (v.to_be_bytes(), 2),
            DmData::I16(v) => (v.to_be_bytes(), 2),
        }
    }
}

/// CEDV profile for the T-Deck-Pro's 1400 mAh LiPo.
///
/// Coefficients from the T-Deck-Pro reference driver (T-Deck-Pro/lib/BQ27220),
/// capacity corrected to 1400 mAh (reference used 1500 mAh from a different product).
/// GaugingConfig bits: CCT|SC|FIXED_EDV0|FCC_LIM|FC_FOR_VDQ|IGNORE_SD = 0x0D31.
pub const CEDV_PROFILE_1400MAH: &[DmEntry] = &[
    DmEntry { address: 0x929B, data: DmData::U16(0x0D31) }, // GaugingConfig
    DmEntry { address: 0x9206, data: DmData::U16(0x0C8C) }, // OperationConfigA
    DmEntry { address: 0x9208, data: DmData::U8(0x4C) },    // OperationConfigB
    DmEntry { address: 0x929D, data: DmData::U16(1400) },   // FullChargeCapacity
    DmEntry { address: 0x929F, data: DmData::U16(1400) },   // DesignCapacity
    DmEntry { address: 0x92A3, data: DmData::U16(3743) },   // EMF
    DmEntry { address: 0x92A9, data: DmData::U16(149) },    // C0
    DmEntry { address: 0x92AB, data: DmData::U16(867) },    // R0
    DmEntry { address: 0x92AD, data: DmData::U16(4030) },   // T0
    DmEntry { address: 0x92AF, data: DmData::U16(316) },    // R1
    DmEntry { address: 0x92B1, data: DmData::U8(9) },       // TC
    DmEntry { address: 0x92B2, data: DmData::U8(0) },       // C1
    DmEntry { address: 0x92BD, data: DmData::U16(4183) },   // StartDOD0
    DmEntry { address: 0x92BF, data: DmData::U16(4043) },   // StartDOD10
    DmEntry { address: 0x92C1, data: DmData::U16(3925) },   // StartDOD20
    DmEntry { address: 0x92C3, data: DmData::U16(3821) },   // StartDOD30
    DmEntry { address: 0x92C5, data: DmData::U16(3725) },   // StartDOD40
    DmEntry { address: 0x92C7, data: DmData::U16(3665) },   // StartDOD50
    DmEntry { address: 0x92C9, data: DmData::U16(3619) },   // StartDOD60
    DmEntry { address: 0x92CB, data: DmData::U16(3585) },   // StartDOD70
    DmEntry { address: 0x92CD, data: DmData::U16(3515) },   // StartDOD80
    DmEntry { address: 0x92CF, data: DmData::U16(3439) },   // StartDOD90
    DmEntry { address: 0x92D1, data: DmData::U16(3299) },   // StartDOD100
    DmEntry { address: 0x92B4, data: DmData::U16(3300) },   // EDV0
    DmEntry { address: 0x92B7, data: DmData::U16(3321) },   // EDV1
    DmEntry { address: 0x92BA, data: DmData::U16(3355) },   // EDV2
    DmEntry { address: 0x91DE, data: DmData::U8(1) },       // Calibration CurrentDeadband
    DmEntry { address: 0x9217, data: DmData::I16(1) },      // PowerSleepCurrent
];

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

/// Decode OperationStatus into `(initcomp, cfg_update)`.
/// Bit 5 = INITCOMP, bit 10 = CFGUPDATE.
pub fn parse_operation_status(raw: [u8; 2]) -> (bool, bool) {
    let bits = u16::from_le_bytes(raw);
    let initcomp = bits & (1 << 5) != 0;
    let cfg_update = bits & (1 << 10) != 0;
    (initcomp, cfg_update)
}

/// Compute the BQ27220 Data Memory write checksum.
///
/// = 0xFF - (addr_lo + addr_hi + data_bytes_big_endian...)
/// Written to REG_MAC_DATA_SUM (0x60) to commit a DM write.
pub fn dm_checksum(address: u16, data_bytes: &[u8]) -> u8 {
    let sum = (address as u8).wrapping_add((address >> 8) as u8);
    let sum = data_bytes.iter().fold(sum, |acc, &b| acc.wrapping_add(b));
    0xFF_u8.wrapping_sub(sum)
}

/// Total packet length for the length byte written to REG_MAC_DATA_SUM + 1.
/// = 2 (address) + data_len + 1 (checksum) + 1 (length itself).
pub fn dm_packet_length(data_len: usize) -> u8 {
    (data_len + 4) as u8
}

/// In-memory test double for [`BatteryDriver`]. Default state is a
/// healthy mid-charge battery; tests can override the fields directly.
pub struct MockBatteryDriver {
    pub level: Result<u8, BatteryError>,
    pub charge_state: Result<ChargeState, BatteryError>,
    pub diag: Result<BatteryDiag, BatteryError>,
    pub provision_result: Result<(), BatteryError>,
}

impl MockBatteryDriver {
    pub fn new() -> Self {
        Self {
            level: Ok(75),
            charge_state: Ok(ChargeState::Discharging),
            diag: Ok(BatteryDiag {
                initcomp: true,
                cfg_update: false,
                design_capacity_mah: 1400,
                full_charge_capacity_mah: 1400,
                remaining_capacity_mah: 1050,
                voltage_mv: 3842,
                current_ma: 245,
            }),
            provision_result: Ok(()),
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

    fn diag(&mut self) -> Result<BatteryDiag, BatteryError> {
        self.diag.clone()
    }

    fn provision(&mut self) -> Result<(), BatteryError> {
        self.provision_result
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

    #[test]
    fn operation_status_initcomp_set() {
        // INITCOMP is bit 5 of the low byte
        assert_eq!(parse_operation_status([0b0010_0000, 0x00]), (true, false));
    }

    #[test]
    fn operation_status_cfgupdate_set() {
        // CFGUPDATE is bit 10 → high byte bit 2
        assert_eq!(parse_operation_status([0x00, 0b0000_0100]), (false, true));
    }

    #[test]
    fn operation_status_both_set() {
        assert_eq!(parse_operation_status([0b0010_0000, 0b0000_0100]), (true, true));
    }

    #[test]
    fn operation_status_neither_set() {
        assert_eq!(parse_operation_status([0x00, 0x00]), (false, false));
    }

    #[test]
    fn operation_status_ignores_unrelated_bits() {
        // SEC bits (bits 1:2) and others set, INITCOMP/CFGUPDATE clear
        assert_eq!(parse_operation_status([0b0000_0110, 0x00]), (false, false));
    }

    #[test]
    fn dm_checksum_u16() {
        // DesignCapacity at 0x929F, value 1400 (0x0578 → big-endian bytes [0x05, 0x78])
        // sum = 0x9F(159) + 0x92(146) + 0x05(5) + 0x78(120) = 430 = 0x1AE → 0xAE mod 256
        // checksum = 0xFF - 0xAE = 0x51
        assert_eq!(dm_checksum(0x929F, &[0x05, 0x78]), 0x51);
    }

    #[test]
    fn dm_checksum_u8() {
        // CurrentDeadband at 0x91DE, value 1 → bytes [0x01]
        // sum = 0xDE(222) + 0x91(145) + 0x01(1) = 368 = 0x170 → 0x70 mod 256
        // checksum = 0xFF - 0x70 = 0x8F
        assert_eq!(dm_checksum(0x91DE, &[0x01]), 0x8F);
    }

    #[test]
    fn dm_packet_length_u8() {
        assert_eq!(dm_packet_length(1), 5); // 2 + 1 + 1 + 1
    }

    #[test]
    fn dm_packet_length_u16() {
        assert_eq!(dm_packet_length(2), 6); // 2 + 2 + 1 + 1
    }

    #[test]
    fn dm_data_u8_be_bytes() {
        let (bytes, len) = DmData::U8(0xAB).be_bytes();
        assert_eq!(len, 1);
        assert_eq!(bytes[0], 0xAB);
    }

    #[test]
    fn dm_data_u16_be_bytes() {
        let (bytes, len) = DmData::U16(0x0578).be_bytes();
        assert_eq!(len, 2);
        assert_eq!(bytes, [0x05, 0x78]);
    }

    #[test]
    fn dm_data_i16_be_bytes() {
        let (bytes, len) = DmData::I16(1).be_bytes();
        assert_eq!(len, 2);
        assert_eq!(bytes, [0x00, 0x01]);
    }

    #[test]
    fn cedv_profile_has_correct_design_capacity() {
        let entry = CEDV_PROFILE_1400MAH
            .iter()
            .find(|e| e.address == 0x929F)
            .expect("DesignCapacity entry missing");
        assert!(matches!(entry.data, DmData::U16(1400)));
    }

    #[test]
    fn cedv_profile_has_correct_full_charge_capacity() {
        let entry = CEDV_PROFILE_1400MAH
            .iter()
            .find(|e| e.address == 0x929D)
            .expect("FullChargeCapacity entry missing");
        assert!(matches!(entry.data, DmData::U16(1400)));
    }
}
