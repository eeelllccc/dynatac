// Platform-independent key mapping and modifier state for the T-Deck-Pro keyboard.
//
// The TCA8418 is configured as a 4-row × 10-column key matrix.
// Key events arrive as (row, col, pressed) tuples from the hardware driver.
// This module handles:
//   - Mapping (row, col) to characters via KEYMAP / SYM_MAP
//   - Tracking Shift and Sym toggle state
//   - Translating raw events into KeyEvent values
//
// Callee invariants:
//   - Only press events produce KeyEvents; releases are consumed.
//   - Shift/Sym presses toggle internal state and return None.

pub const ROWS: u8 = 4;
pub const COLS: u8 = 10;

// --- Key map -----------------------------------------------------------------
//
// Special entries:
//   '\x08'  → Backspace
//   '\n'    → Enter
//   '\x01'  → Shift modifier (toggle)
//   '\x02'  → Sym modifier (toggle)
//   '\x03'  → Mic (no-op for now)
//   '\0'    → unmapped / no key

#[rustfmt::skip]
const KEYMAP: [[char; COLS as usize]; ROWS as usize] = [
    // col: 0    1    2    3    4    5    6    7    8    9
    ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i',  'o',    'p'   ],  // row 0
    ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k',  'l',    '\x08'],  // row 1  (col9=backspace)
    ['\0','z', 'x', 'c', 'v', 'b', 'n', 'm',  '$',    '\n'  ],  // row 2  (col0=alt, col8=$, col9=enter)
    ['\0','\0','\0','\0','\0', '\x01','\x03',' ',  '\x02','\x01'], // row 3
];

#[rustfmt::skip]
const SYM_MAP: [[char; COLS as usize]; ROWS as usize] = [
    ['#', '1', '2', '3', '(', ')', '_', '-',  '+',    '@'   ],  // row 0
    ['*', '4', '5', '6', '/', ':', ';', '\'', '"',    '\x08'],  // row 1
    ['\0','7', '8', '9', '?', '!', '`', '.',  '$',    '\n'  ],  // row 2
    ['\0','\0','\0','\0','\0', '\x01','\x03',' ',  '\x02','\x01'], // row 3
];

/// A decoded key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEvent {
    /// A printable character (space, letter, punctuation).
    Char(char),
    /// The backspace key was pressed.
    Backspace,
    /// The enter key was pressed.
    Enter,
}

/// Platform-independent key mapper and modifier state machine.
///
/// Translates raw (row, col, pressed) events from the TCA8418 hardware
/// into `KeyEvent` values, handling Shift/Sym toggle logic internally.
pub struct KeyMapper {
    shift_on: bool,
    sym_on: bool,
}

impl KeyMapper {
    pub fn new() -> Self {
        KeyMapper {
            shift_on: false,
            sym_on: false,
        }
    }

    /// Whether the Shift toggle is currently active.
    pub fn shift_on(&self) -> bool {
        self.shift_on
    }

    /// Whether the Sym toggle is currently active.
    pub fn sym_on(&self) -> bool {
        self.sym_on
    }

    /// Process a raw key event from the hardware.
    ///
    /// `row` and `col` are after the hardware column reversal
    /// (col = (COLS-1) - raw_col). `pressed` is true for press, false for release.
    ///
    /// Returns `Some(KeyEvent)` for actionable presses, `None` for releases,
    /// modifier toggles, and unmapped keys.
    pub fn process(&mut self, row: u8, col: u8, pressed: bool) -> Option<KeyEvent> {
        if !pressed {
            return None;
        }

        if row >= ROWS || col >= COLS {
            return None;
        }

        let base = KEYMAP[row as usize][col as usize];
        match base {
            '\x01' => {
                self.shift_on = !self.shift_on;
                None
            }
            '\x02' => {
                self.sym_on = !self.sym_on;
                None
            }
            '\x03' | '\0' => None,
            '\x08' => Some(KeyEvent::Backspace),
            '\n' => Some(KeyEvent::Enter),
            c => {
                let ch = if self.sym_on {
                    SYM_MAP[row as usize][col as usize]
                } else if self.shift_on {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                Some(KeyEvent::Char(ch))
            }
        }
    }
}

impl Default for KeyMapper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_letter_press() {
        let mut km = KeyMapper::new();
        // row=0, col=0 = 'q'
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('q')));
    }

    #[test]
    fn release_returns_none() {
        let mut km = KeyMapper::new();
        assert_eq!(km.process(0, 0, false), None);
    }

    #[test]
    fn backspace_event() {
        let mut km = KeyMapper::new();
        // row=1, col=9 = '\x08' = Backspace
        assert_eq!(km.process(1, 9, true), Some(KeyEvent::Backspace));
    }

    #[test]
    fn enter_event() {
        let mut km = KeyMapper::new();
        // row=2, col=9 = '\n' = Enter
        assert_eq!(km.process(2, 9, true), Some(KeyEvent::Enter));
    }

    #[test]
    fn space_bar() {
        let mut km = KeyMapper::new();
        // row=3, col=7 = ' '
        assert_eq!(km.process(3, 7, true), Some(KeyEvent::Char(' ')));
    }

    #[test]
    fn shift_toggles_uppercase() {
        let mut km = KeyMapper::new();
        assert!(!km.shift_on());

        // Press Shift (row=3, col=5)
        assert_eq!(km.process(3, 5, true), None);
        assert!(km.shift_on());

        // Now 'q' should be 'Q'
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('Q')));

        // Press Shift again to toggle off
        assert_eq!(km.process(3, 5, true), None);
        assert!(!km.shift_on());

        // 'q' should be lowercase again
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('q')));
    }

    #[test]
    fn sym_produces_symbols() {
        let mut km = KeyMapper::new();

        // Press Sym (row=3, col=8)
        assert_eq!(km.process(3, 8, true), None);
        assert!(km.sym_on());

        // row=0, col=0 should be '#' (SYM_MAP)
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('#')));
        // row=0, col=1 should be '1'
        assert_eq!(km.process(0, 1, true), Some(KeyEvent::Char('1')));
        // row=1, col=1 should be '4'
        assert_eq!(km.process(1, 1, true), Some(KeyEvent::Char('4')));
    }

    #[test]
    fn sym_overrides_shift() {
        let mut km = KeyMapper::new();

        // Turn both on
        km.process(3, 5, true); // Shift
        km.process(3, 8, true); // Sym
        assert!(km.shift_on());
        assert!(km.sym_on());

        // Sym takes priority: row=0 col=0 → '#' not 'Q'
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('#')));
    }

    #[test]
    fn unmapped_keys_return_none() {
        let mut km = KeyMapper::new();
        // row=2, col=0 = '\0' (alt, unmapped)
        assert_eq!(km.process(2, 0, true), None);
        // row=3, col=0 = '\0'
        assert_eq!(km.process(3, 0, true), None);
    }

    #[test]
    fn mic_returns_none() {
        let mut km = KeyMapper::new();
        // row=3, col=6 = '\x03' (Mic)
        assert_eq!(km.process(3, 6, true), None);
    }

    #[test]
    fn out_of_range_returns_none() {
        let mut km = KeyMapper::new();
        assert_eq!(km.process(4, 0, true), None);  // row too high
        assert_eq!(km.process(0, 10, true), None); // col too high
    }

    #[test]
    fn dollar_sign() {
        let mut km = KeyMapper::new();
        // row=2, col=8 = '$'
        assert_eq!(km.process(2, 8, true), Some(KeyEvent::Char('$')));
    }

    #[test]
    fn shift_does_not_affect_non_letters() {
        let mut km = KeyMapper::new();
        km.process(3, 5, true); // Shift on

        // '$' is not a letter — shift calls to_ascii_uppercase which is no-op
        assert_eq!(km.process(2, 8, true), Some(KeyEvent::Char('$')));
        // space stays space
        assert_eq!(km.process(3, 7, true), Some(KeyEvent::Char(' ')));
    }

    #[test]
    fn backspace_and_enter_unaffected_by_modifiers() {
        let mut km = KeyMapper::new();
        km.process(3, 5, true); // Shift
        km.process(3, 8, true); // Sym

        assert_eq!(km.process(1, 9, true), Some(KeyEvent::Backspace));
        assert_eq!(km.process(2, 9, true), Some(KeyEvent::Enter));
    }

    #[test]
    fn spot_check_several_keys() {
        let mut km = KeyMapper::new();
        assert_eq!(km.process(0, 4, true), Some(KeyEvent::Char('t')));
        assert_eq!(km.process(0, 9, true), Some(KeyEvent::Char('p')));
        assert_eq!(km.process(1, 0, true), Some(KeyEvent::Char('a')));
        assert_eq!(km.process(1, 7, true), Some(KeyEvent::Char('k')));
        assert_eq!(km.process(2, 7, true), Some(KeyEvent::Char('m')));
    }
}
