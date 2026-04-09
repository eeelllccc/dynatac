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
//   - An I2C master for GPIO13/GPIO14 must be initialised before
//     constructing `Keyboard`.
//
// Callee invariants:
//   - The FIFO is flushed during `new()`.
//   - Only key-press events are surfaced; releases are silently consumed.

use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::hal::sys::EspError;

// --- TCA8418 I2C address and registers ----------------------------------------

const ADDR: u8 = 0x34;

const REG_CFG: u8 = 0x01;
const REG_INT_STAT: u8 = 0x02;
const REG_KEY_LCK_EC: u8 = 0x03;
const REG_KEY_EVENT_A: u8 = 0x04;
const REG_KP_GPIO_1: u8 = 0x1D;
const REG_KP_GPIO_2: u8 = 0x1E;

// --- Matrix dimensions --------------------------------------------------------

const ROWS: u8 = 4;
const COLS: u8 = 10;

// --- Key map -----------------------------------------------------------------
//
// Columns are hardware-reversed: col index = (COLS-1) - (code % COLS).
//
// Special entries:
//   '\x08'  → Backspace
//   '\n'    → Enter
//   '\x01'  → Shift modifier (toggle)
//   '\x02'  → Sym modifier (toggle)
//   '\x03'  → Mic (no-op for now)
//   '\0'    → unmapped / no key
//
// Note: The space bar is wired to row 3 col 7, not cols 0-4 as the
// factory firmware suggests.  Cols 0-4 of row 3 are unused.

#[rustfmt::skip]
const KEYMAP: [[char; COLS as usize]; ROWS as usize] = [
    // col: 0    1    2    3    4    5    6    7    8    9
    ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i',  'o',    'p'   ],  // row 0
    ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k',  'l',    '\x08'],  // row 1  (col9=backspace)
    ['\0','z', 'x', 'c', 'v', 'b', 'n', 'm',  '$',    '\n'  ],  // row 2  (col0=alt, col8=$, col9=enter)
    ['\0','\0','\0','\0','\0', '\x01','\x03',' ',  '\x02','\x01'], // row 3  (col5=shift, col6=mic, col7=space, col8=sym, col9=shift)
];

// --- Symbol overlay (active when Sym is toggled on) --------------------------
//
// Maps from special-keys.txt.  Non-letter keys (backspace, enter, $,
// modifiers) keep their KEYMAP values.

#[rustfmt::skip]
const SYM_MAP: [[char; COLS as usize]; ROWS as usize] = [
    ['#', '1', '2', '3', '(', ')', '_', '-',  '+',    '@'   ],  // row 0
    ['*', '4', '5', '6', '/', ':', ';', '\'', '"',    '\x08'],  // row 1
    ['\0','7', '8', '9', '?', '!', '`', '.',  '$',    '\n'  ],  // row 2
    ['\0','\0','\0','\0','\0', '\x01','\x03',' ',  '\x02','\x01'], // row 3
];

// --- Public key event type ----------------------------------------------------

/// A decoded key press from the T-Deck-Pro keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// A printable character (space, letter, punctuation).
    Char(char),
    /// The backspace key was pressed.
    Backspace,
    /// The enter key was pressed.
    Enter,
}

// --- Driver -------------------------------------------------------------------

/// Driver for the TCA8418 keyboard on the T-Deck-Pro.
///
/// Provides a polling interface: call `poll()` in a loop to drain
/// buffered key-press events one at a time.
///
/// Tracks Shift and Sym toggle state internally.  When Shift is on,
/// letter keys produce uppercase.  When Sym is on, keys produce the
/// symbol from `SYM_MAP`.  Pressing Shift or Sym again turns the
/// mode off.  The two modes are independent; if both are active, Sym
/// takes priority (the symbol map already specifies exact characters).
pub struct Keyboard<'d> {
    i2c: I2cDriver<'d>,
    shift_on: bool,
    sym_on: bool,
}

impl<'d> Keyboard<'d> {
    /// Initialise the TCA8418 as a 4×10 key matrix and flush the FIFO.
    ///
    /// Caller must have created `i2c` on SDA=GPIO13, SCL=GPIO14.
    pub fn new(i2c: I2cDriver<'d>) -> Result<Self, EspError> {
        let mut kb = Keyboard {
            i2c,
            shift_on: false,
            sym_on: false,
        };
        kb.configure_matrix()?;
        kb.flush()?;
        Ok(kb)
    }

    /// Whether the Shift toggle is currently active.
    pub fn shift_on(&self) -> bool {
        self.shift_on
    }

    /// Whether the Sym toggle is currently active.
    pub fn sym_on(&self) -> bool {
        self.sym_on
    }

    /// Poll the FIFO for one key-press event.
    ///
    /// Returns `None` when the FIFO is empty.  Release events are
    /// silently consumed.  Shift and Sym presses toggle internal state
    /// and return `None`.  Printable characters are transformed
    /// according to the current Shift/Sym state.
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

            if !pressed {
                continue; // skip releases, read next event
            }

            let row = code / COLS;
            let col = (COLS - 1) - (code % COLS); // hardware column reversal

            if row >= ROWS {
                log::debug!("key code {} out of range (row={})", code, row);
                continue;
            }

            let base = KEYMAP[row as usize][col as usize];
            match base {
                '\x01' => {
                    self.shift_on = !self.shift_on;
                    log::info!("shift {}", if self.shift_on { "ON" } else { "OFF" });
                    continue; // consumed internally
                }
                '\x02' => {
                    self.sym_on = !self.sym_on;
                    log::info!("sym {}", if self.sym_on { "ON" } else { "OFF" });
                    continue; // consumed internally
                }
                '\x03' | '\0' => {
                    log::debug!("modifier/unmapped key row={} col={}", row, col);
                    continue; // consumed internally
                }
                '\x08' => return Ok(Some(KeyEvent::Backspace)),
                '\n' => return Ok(Some(KeyEvent::Enter)),
                c => {
                    let ch = if self.sym_on {
                        SYM_MAP[row as usize][col as usize]
                    } else if self.shift_on {
                        c.to_ascii_uppercase()
                    } else {
                        c
                    };
                    return Ok(Some(KeyEvent::Char(ch)));
                }
            }
        }
    }

    // --- I2C helpers ----------------------------------------------------------

    fn read_reg(&mut self, reg: u8) -> Result<u8, EspError> {
        let mut buf = [0u8; 1];
        self.i2c.write_read(ADDR, &[reg], &mut buf, 100)?;
        Ok(buf[0])
    }

    fn write_reg(&mut self, reg: u8, val: u8) -> Result<(), EspError> {
        self.i2c.write(ADDR, &[reg, val], 100)?;
        Ok(())
    }

    /// Configure TCA8418 rows 0-3 and cols 0-9 as keypad-scan pins.
    fn configure_matrix(&mut self) -> Result<(), EspError> {
        // KP_GPIO_1: rows 0-7 — set bits 0..3 (rows 0-3 as keypad)
        let row_mask: u8 = (1 << ROWS) - 1; // 0x0F
        self.write_reg(REG_KP_GPIO_1, row_mask)?;

        // KP_GPIO_2: cols 0-7 — all 8 as keypad
        self.write_reg(REG_KP_GPIO_2, 0xFF)?;

        // KP_GPIO_3 bits 0-1 → cols 8-9
        let col_extra: u8 = (1 << (COLS - 8)) - 1; // 0x03
        self.write_reg(REG_KP_GPIO_2 + 1, col_extra)?;

        // Enable key-event interrupt in config register
        self.write_reg(REG_CFG, 0x01)?;

        Ok(())
    }

    /// Drain all pending events from the FIFO.
    fn flush(&mut self) -> Result<(), EspError> {
        loop {
            let count = self.read_reg(REG_KEY_LCK_EC)? & 0x0F;
            if count == 0 {
                break;
            }
            let _ = self.read_reg(REG_KEY_EVENT_A)?;
        }
        // Clear all interrupt flags
        self.write_reg(REG_INT_STAT, 0x0F)?;
        Ok(())
    }
}
