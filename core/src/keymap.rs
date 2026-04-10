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
//   '\x03'  → Mic key. Bare press is a no-op; Sym+Mic produces '0'
//             (the digit isn't reachable on the letter rows, where Sym
//             covers 1–9 only).
//   '\0'    → unmapped / no key

#[rustfmt::skip]
const KEYMAP: [[char; COLS as usize]; ROWS as usize] = [
    // col: 0    1    2    3    4    5    6    7    8    9
    ['q', 'w', 'e', 'r', 't', 'y', 'u', 'i',  'o',    'p'   ],  // row 0
    ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k',  'l',    '\x08'],  // row 1  (col9=backspace)
    ['\x04','z', 'x', 'c', 'v', 'b', 'n', 'm',  '$',    '\n'  ],  // row 2  (col0=alt, col8=$, col9=enter)
    ['\0','\0','\0','\0','\0', '\x01','\x03',' ',  '\x02','\x01'], // row 3
];

#[rustfmt::skip]
const SYM_MAP: [[char; COLS as usize]; ROWS as usize] = [
    ['#', '1', '2', '3', '(', ')', '_', '-',  '+',    '@'   ],  // row 0
    ['*', '4', '5', '6', '/', ':', ';', '\'', '"',    '\x08'],  // row 1
    ['\x04','7', '8', '9', '?', '!', '`', '.',  '$',    '\n'  ],  // row 2
    ['\0','\0','\0','\0','\0', '\x01','0',' ',  '\x02','\x01'], // row 3  (col6=sym+mic → '0')
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
    /// Shift+Enter: insert a newline into the input buffer instead of submitting.
    SoftEnter,
    /// Alt+Backspace: cancel the current operation. In interactive list /
    /// text-prompt mode this exits the mode; at the regular prompt it
    /// clears the input line.
    Cancel,
    /// Scroll the viewport up (Alt + y).
    ScrollUp,
    /// Scroll the viewport down (Alt + h).
    ScrollDown,
    /// Jump to the bottom of output (Alt + b).
    ScrollBottom,
}

/// Platform-independent key mapper and modifier state machine.
///
/// Translates raw (row, col, pressed) events from the TCA8418 hardware
/// into `KeyEvent` values, handling Shift/Sym toggle logic internally.
pub struct KeyMapper {
    shift_on: bool,
    sym_on: bool,
    alt_on: bool,
}

impl KeyMapper {
    pub fn new() -> Self {
        KeyMapper {
            shift_on: false,
            sym_on: false,
            alt_on: false,
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

    /// Whether the Alt toggle is currently active.
    pub fn alt_on(&self) -> bool {
        self.alt_on
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
            '\x03' => {
                // Mic key: bare press is a no-op; Sym+Mic is the only way
                // to type '0' on this keyboard (Sym mode covers 1–9 on
                // the letter rows but leaves 0 without a home). The `c =>`
                // arm below does the normal Sym lookup for letter keys,
                // but this key has no base character so we have to handle
                // the Sym branch here explicitly — driven by SYM_MAP so
                // the sym table remains the single source of truth.
                if self.sym_on {
                    Some(KeyEvent::Char(SYM_MAP[row as usize][col as usize]))
                } else {
                    None
                }
            }
            '\0' => None,
            '\x04' => {
                self.alt_on = !self.alt_on;
                None
            }
            '\x08' => {
                if self.alt_on {
                    // Alt+Backspace cancels the current operation. Auto-clears
                    // Alt so the next keystroke isn't accidentally consumed by
                    // an Alt-action.
                    self.alt_on = false;
                    Some(KeyEvent::Cancel)
                } else {
                    Some(KeyEvent::Backspace)
                }
            }
            '\n' => {
                if self.shift_on {
                    // Shift+Enter: insert newline in buffer; auto-clear Shift
                    // (only Enter consumes the Shift toggle — letters do not).
                    self.shift_on = false;
                    Some(KeyEvent::SoftEnter)
                } else {
                    Some(KeyEvent::Enter)
                }
            }
            c => {
                if self.alt_on {
                    match c {
                        'y' => return Some(KeyEvent::ScrollUp),
                        'h' => return Some(KeyEvent::ScrollDown),
                        'b' => return Some(KeyEvent::ScrollBottom),
                        _ => {}
                    }
                }
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
        // row=3, col=0 = '\0'
        assert_eq!(km.process(3, 0, true), None);
    }

    #[test]
    fn alt_toggles() {
        let mut km = KeyMapper::new();
        assert!(!km.alt_on());
        // row=2, col=0 = Alt toggle
        assert_eq!(km.process(2, 0, true), None);
        assert!(km.alt_on());
        assert_eq!(km.process(2, 0, true), None);
        assert!(!km.alt_on());
    }

    #[test]
    fn alt_y_produces_scroll_up() {
        let mut km = KeyMapper::new();
        km.process(2, 0, true); // Alt on
        // 'y' is row=0, col=5
        assert_eq!(km.process(0, 5, true), Some(KeyEvent::ScrollUp));
    }

    #[test]
    fn alt_h_produces_scroll_down() {
        let mut km = KeyMapper::new();
        km.process(2, 0, true); // Alt on
        // 'h' is row=1, col=5
        assert_eq!(km.process(1, 5, true), Some(KeyEvent::ScrollDown));
    }

    #[test]
    fn alt_b_produces_scroll_bottom() {
        let mut km = KeyMapper::new();
        km.process(2, 0, true); // Alt on
        // 'b' is row=2, col=5
        assert_eq!(km.process(2, 5, true), Some(KeyEvent::ScrollBottom));
    }

    #[test]
    fn alt_other_keys_pass_through() {
        let mut km = KeyMapper::new();
        km.process(2, 0, true); // Alt on
        // 'q' is row=0, col=0 — not a scroll key, passes through
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('q')));
    }

    #[test]
    fn mic_returns_none() {
        let mut km = KeyMapper::new();
        // row=3, col=6 = '\x03' (Mic)
        assert_eq!(km.process(3, 6, true), None);
    }

    #[test]
    fn sym_mic_produces_zero() {
        let mut km = KeyMapper::new();
        km.process(3, 8, true); // Sym on
        // row=3, col=6 = Mic
        assert_eq!(km.process(3, 6, true), Some(KeyEvent::Char('0')));
        // Sym stays on, consistent with all other sym keys — so the user
        // can type "000" (or "100", "0800…") without re-toggling Sym.
        assert!(km.sym_on());
        assert_eq!(km.process(3, 6, true), Some(KeyEvent::Char('0')));
        assert_eq!(km.process(3, 6, true), Some(KeyEvent::Char('0')));
    }

    #[test]
    fn alt_mic_does_nothing() {
        let mut km = KeyMapper::new();
        km.process(2, 0, true); // Alt on
        // Mic with only Alt (no Sym) is a no-op.
        assert_eq!(km.process(3, 6, true), None);
    }

    #[test]
    fn mic_without_sym_is_no_op_after_sym_mic() {
        let mut km = KeyMapper::new();
        km.process(3, 8, true); // Sym on
        km.process(3, 6, true); // Sym+Mic → '0'
        km.process(3, 8, true); // Sym off
        // Bare Mic is a no-op again.
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
    fn backspace_unaffected_by_modifiers() {
        let mut km = KeyMapper::new();
        km.process(3, 5, true); // Shift
        km.process(3, 8, true); // Sym

        assert_eq!(km.process(1, 9, true), Some(KeyEvent::Backspace));
    }

    #[test]
    fn shift_enter_produces_soft_enter_and_clears_shift() {
        let mut km = KeyMapper::new();
        km.process(3, 5, true); // Shift on
        assert!(km.shift_on());

        // Shift+Enter → SoftEnter
        assert_eq!(km.process(2, 9, true), Some(KeyEvent::SoftEnter));
        // Shift auto-cleared by Enter
        assert!(!km.shift_on());

        // Next Enter is a normal Enter
        assert_eq!(km.process(2, 9, true), Some(KeyEvent::Enter));
    }

    #[test]
    fn plain_enter_unaffected() {
        let mut km = KeyMapper::new();
        assert_eq!(km.process(2, 9, true), Some(KeyEvent::Enter));
    }

    #[test]
    fn alt_backspace_produces_cancel_and_clears_alt() {
        let mut km = KeyMapper::new();
        // Plain backspace is unchanged
        assert_eq!(km.process(1, 9, true), Some(KeyEvent::Backspace));

        // Alt on
        km.process(2, 0, true);
        assert!(km.alt_on());

        // Alt+Backspace → Cancel
        assert_eq!(km.process(1, 9, true), Some(KeyEvent::Cancel));
        // Alt auto-cleared
        assert!(!km.alt_on());

        // Next backspace is plain again
        assert_eq!(km.process(1, 9, true), Some(KeyEvent::Backspace));
    }

    #[test]
    fn shift_persists_through_letters_then_clears_only_on_enter() {
        let mut km = KeyMapper::new();
        km.process(3, 5, true); // Shift on
        // Letters use shift but do NOT clear it
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('Q')));
        assert!(km.shift_on());
        assert_eq!(km.process(0, 1, true), Some(KeyEvent::Char('W')));
        assert!(km.shift_on());

        // Shift+Enter clears
        assert_eq!(km.process(2, 9, true), Some(KeyEvent::SoftEnter));
        assert!(!km.shift_on());

        // Next letter is lowercase
        assert_eq!(km.process(0, 0, true), Some(KeyEvent::Char('q')));
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
