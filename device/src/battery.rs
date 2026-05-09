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

use std::thread::sleep;
use std::time::Duration;

use dynatac_core::battery::{
    dm_checksum, dm_packet_length, parse_battery_status, parse_operation_status, parse_soc,
    BatteryDiag, BatteryDriver, BatteryError, ChargeState, DmData, CEDV_PROFILE_1400MAH,
    CTRL_ENTER_CFG_UPDATE, CTRL_EXIT_CFG_UPDATE_REINIT, CTRL_FULL_ACCESS_KEY, CTRL_RESET,
    CTRL_SEALED, CTRL_UNSEAL_KEY1, CTRL_UNSEAL_KEY2, REG_BATTERY_STATUS, REG_CONTROL,
    REG_CURRENT, REG_DESIGN_CAPACITY, REG_FULL_CHARGE_CAPACITY, REG_MAC_DATA_SUM,
    REG_OPERATION_STATUS, REG_REMAINING_CAPACITY, REG_SELECT_SUBCLASS, REG_STATE_OF_CHARGE,
    REG_VOLTAGE,
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

    // Write a 2-byte sub-command to CommandControl (0x00), little-endian.
    fn write_control(&self, sub_cmd: u16) -> Result<(), BatteryError> {
        let [lo, hi] = sub_cmd.to_le_bytes();
        self.dev.write(&[REG_CONTROL, lo, hi]).map_err(|e| {
            log::warn!("battery write_control 0x{:04X} failed: {:?}", sub_cmd, e);
            BatteryError::Bus
        })
    }

    // Write one CEDV Data Memory parameter using the BQ27220 DM write protocol.
    // Data bytes are written big-endian to the MAC buffer.
    fn write_dm(&self, address: u16, data: DmData) -> Result<(), BatteryError> {
        let (be_bytes, len) = data.be_bytes();
        let data_slice = &be_bytes[..len];

        let addr_lo = (address & 0xFF) as u8;
        let addr_hi = (address >> 8) as u8;

        // Build the subclass select packet: [REG, addr_lo, addr_hi, data...]
        let mut pkt = [0u8; 6]; // REG + 2 addr + up to 2 data bytes, plus spare
        pkt[0] = REG_SELECT_SUBCLASS;
        pkt[1] = addr_lo;
        pkt[2] = addr_hi;
        pkt[3..3 + len].copy_from_slice(data_slice);
        self.dev.write(&pkt[..3 + len]).map_err(|e| {
            log::warn!("battery write_dm addr=0x{:04X} write failed: {:?}", address, e);
            BatteryError::Bus
        })?;
        sleep(Duration::from_millis(1)); // ≥250µs

        let checksum = dm_checksum(address, data_slice);
        let length = dm_packet_length(len);
        self.dev
            .write(&[REG_MAC_DATA_SUM, checksum, length])
            .map_err(|e| {
                log::warn!("battery write_dm addr=0x{:04X} checksum failed: {:?}", address, e);
                BatteryError::Bus
            })?;
        sleep(Duration::from_millis(10)); // ≥10ms for DM write to settle

        Ok(())
    }

    // Poll OperationStatus (0x3A) until INITCOMP (bit 5) is set, or timeout.
    fn wait_initcomp(&self, timeout_ms: u32) -> Result<(), BatteryError> {
        for _ in 0..timeout_ms {
            let raw = self.read_u16(REG_OPERATION_STATUS)?;
            let (initcomp, _) = parse_operation_status(raw);
            if initcomp {
                return Ok(());
            }
            sleep(Duration::from_millis(1));
        }
        log::warn!("battery: INITCOMP timeout after {}ms", timeout_ms);
        Err(BatteryError::Timeout)
    }

    // Poll OperationStatus until CFGUPDATE (bit 10) is set, or timeout.
    fn wait_cfgupdate_set(&self, timeout_ms: u32) -> Result<(), BatteryError> {
        for _ in 0..timeout_ms {
            let raw = self.read_u16(REG_OPERATION_STATUS)?;
            let (_, cfg_update) = parse_operation_status(raw);
            if cfg_update {
                return Ok(());
            }
            sleep(Duration::from_millis(1));
        }
        log::warn!("battery: CFGUPDATE set timeout after {}ms", timeout_ms);
        Err(BatteryError::Timeout)
    }

    // Poll OperationStatus until CFGUPDATE (bit 10) is clear, or timeout.
    fn wait_cfgupdate_clear(&self, timeout_ms: u32) -> Result<(), BatteryError> {
        for _ in 0..timeout_ms {
            let raw = self.read_u16(REG_OPERATION_STATUS)?;
            let (_, cfg_update) = parse_operation_status(raw);
            if !cfg_update {
                return Ok(());
            }
            sleep(Duration::from_millis(1));
        }
        log::warn!("battery: CFGUPDATE clear timeout after {}ms", timeout_ms);
        Err(BatteryError::Timeout)
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

    fn provision(&mut self) -> Result<(), BatteryError> {
        log::info!("BQ27220: starting provisioning");

        // Unseal (two key writes with a delay between each).
        self.write_control(CTRL_UNSEAL_KEY1)?;
        sleep(Duration::from_millis(5));
        self.write_control(CTRL_UNSEAL_KEY2)?;
        sleep(Duration::from_millis(5));

        // Escalate to full access (write key twice).
        self.write_control(CTRL_FULL_ACCESS_KEY)?;
        sleep(Duration::from_millis(5));
        self.write_control(CTRL_FULL_ACCESS_KEY)?;
        sleep(Duration::from_millis(5));

        // Reset — clears any stale state and reinitialises from Data Memory.
        log::info!("BQ27220: resetting");
        self.write_control(CTRL_RESET)?;
        self.wait_initcomp(4000)?; // up to 4s per reference driver

        // Enter config-update mode so we can write Data Memory.
        log::info!("BQ27220: entering CFG_UPDATE mode");
        self.write_control(CTRL_ENTER_CFG_UPDATE)?;
        self.wait_cfgupdate_set(2000)?;

        // Write every CEDV profile parameter.
        log::info!(
            "BQ27220: writing {} DM parameters",
            CEDV_PROFILE_1400MAH.len()
        );
        for entry in CEDV_PROFILE_1400MAH {
            self.write_dm(entry.address, entry.data)?;
        }

        // Exit config-update with reinit so the gauge applies new parameters.
        log::info!("BQ27220: exiting CFG_UPDATE (reinit)");
        self.write_control(CTRL_EXIT_CFG_UPDATE_REINIT)?;
        sleep(Duration::from_millis(2000)); // 2s config-apply delay per reference driver
        self.wait_cfgupdate_clear(2000)?;

        // Reseal.
        self.write_control(CTRL_SEALED)?;

        log::info!("BQ27220: provisioning complete");
        Ok(())
    }

    fn diag(&mut self) -> Result<BatteryDiag, BatteryError> {
        let op_status = self.read_u16(REG_OPERATION_STATUS)?;
        let (initcomp, cfg_update) = parse_operation_status(op_status);
        let design_capacity_mah = u16::from_le_bytes(self.read_u16(REG_DESIGN_CAPACITY)?);
        let full_charge_capacity_mah =
            u16::from_le_bytes(self.read_u16(REG_FULL_CHARGE_CAPACITY)?);
        let remaining_capacity_mah = u16::from_le_bytes(self.read_u16(REG_REMAINING_CAPACITY)?);
        let voltage_mv = u16::from_le_bytes(self.read_u16(REG_VOLTAGE)?);
        let current_raw = self.read_u16(REG_CURRENT)?;
        let current_ma = i16::from_le_bytes(current_raw);
        Ok(BatteryDiag {
            initcomp,
            cfg_update,
            design_capacity_mah,
            full_charge_capacity_mah,
            remaining_capacity_mah,
            voltage_mv,
            current_ma,
        })
    }
}
