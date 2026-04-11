// Driver for the TCA8418 keyboard matrix on the LilyGo T-Deck-Pro.
//
// Hardware connections:
//   I2C SDA → GPIO13
//   I2C SCL → GPIO14
//   IRQ     → GPIO15 (active LOW, directly readable — not used here)
//
// The TCA8418 is configured as a 4-row × 10-column key matrix.
// Key events are read from the FIFO via register 0x04; bit 7
// distinguishes press (1) from release (0), bits 6:0 encode the
// key code (1-based: code = row * COLS + col + 1).
//
// Caller invariants:
//   - An `I2cDevice` bound to the TCA8418 (addr 0x34) on the shared
//     GPIO13/14 bus must be provided.
//
// Callee invariants:
//   - The FIFO is flushed during `new()`.
//   - Only key-press events are surfaced; releases are silently consumed.

use esp_idf_svc::hal::sys::EspError;

use dynatac_core::keymap::{KeyEvent, KeyMapper, COLS, ROWS};

use crate::i2c_bus::I2cDevice;

// --- TCA8418 registers --------------------------------------------------------

const REG_CFG: u8 = 0x01;
const REG_INT_STAT: u8 = 0x02;
const REG_KEY_LCK_EC: u8 = 0x03;
const REG_KEY_EVENT_A: u8 = 0x04;
const REG_KP_GPIO_1: u8 = 0x1D;
const REG_KP_GPIO_2: u8 = 0x1E;

/// Driver for the TCA8418 keyboard on the T-Deck-Pro.
///
/// Wraps a `KeyMapper` for key-to-event translation and manages I2C
/// communication with the TCA8418 hardware via a shared-bus handle.
pub struct Keyboard<'a, 'd> {
    dev: I2cDevice<'a, 'd>,
    mapper: KeyMapper,
}

impl<'a, 'd> Keyboard<'a, 'd> {
    /// Initialise the TCA8418 as a 4×10 key matrix and flush the FIFO.
    pub fn new(dev: I2cDevice<'a, 'd>) -> Result<Self, EspError> {
        let mut kb = Keyboard {
            dev,
            mapper: KeyMapper::new(),
        };
        kb.configure_matrix()?;
        kb.flush()?;
        Ok(kb)
    }

    /// Whether the Shift toggle is currently active.
    pub fn shift_on(&self) -> bool {
        self.mapper.shift_on()
    }

    /// Whether the Sym toggle is currently active.
    pub fn sym_on(&self) -> bool {
        self.mapper.sym_on()
    }

    /// Poll the FIFO for one key-press event.
    pub fn poll(&mut self) -> Result<Option<KeyEvent>, EspError> {
        loop {
            let count = self.read_reg(REG_KEY_LCK_EC)? & 0x0F;
            if count == 0 {
                return Ok(None);
            }

            let raw = self.read_reg(REG_KEY_EVENT_A)?;
            let pressed = raw & 0x80 != 0;
            let code = (raw & 0x7F).wrapping_sub(1); // 0-based key index

            // Clear K_INT only when FIFO is now empty.
            let remaining = self.read_reg(REG_KEY_LCK_EC)? & 0x0F;
            if remaining == 0 {
                self.write_reg(REG_INT_STAT, 0x0F)?;
            }

            let row = code / COLS;
            let col = (COLS - 1) - (code % COLS); // hardware column reversal

            if row >= ROWS {
                log::debug!("key code {} out of range (row={})", code, row);
                continue;
            }

            match self.mapper.process(row, col, pressed) {
                Some(event) => return Ok(Some(event)),
                None => continue,
            }
        }
    }

    // --- I2C helpers ----------------------------------------------------------

    fn read_reg(&mut self, reg: u8) -> Result<u8, EspError> {
        let mut buf = [0u8; 1];
        self.dev.write_read(&[reg], &mut buf)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), EspError> {
        self.dev.write(&[reg, val])?;
        Ok(())
    }

    fn configure_matrix(&mut self) -> Result<(), EspError> {
        let row_mask: u8 = (1 << ROWS) - 1;
        self.write_reg(REG_KP_GPIO_1, row_mask)?;
        self.write_reg(REG_KP_GPIO_2, 0xFF)?;
        let col_extra: u8 = (1 << (COLS - 8)) - 1;
        self.write_reg(REG_KP_GPIO_2 + 1, col_extra)?;
        self.write_reg(REG_CFG, 0x01)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), EspError> {
        loop {
            let count = self.read_reg(REG_KEY_LCK_EC)? & 0x0F;
            if count == 0 {
                break;
            }
            let _ = self.read_reg(REG_KEY_EVENT_A)?;
        }
        self.write_reg(REG_INT_STAT, 0x0F)?;
        Ok(())
    }
}
