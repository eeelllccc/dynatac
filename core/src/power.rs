// Power state machine.
//
// The device has two power states: Active (normal terminal) and
// Locked (lockscreen showing, MCU about to enter light sleep). The
// transitions are driven entirely by key events:
//
//   Active   --Alt+L (KeyEvent::Lock)--> Locked
//   Locked   --Alt+L (KeyEvent::Lock)--> Active
//
// Only Alt+L unlocks. Other key events while Locked are ignored —
// the device-side caller is expected to drain them and re-enter
// light sleep so the user has to deliberately unlock.
//
// This module is pure state — the device-side glue (light sleep,
// EPD power, radio shutdown) lives in `device/src/main.rs` and acts
// on the `PowerAction` returned from `handle_key`.

use crate::keymap::KeyEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Active,
    Locked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// Nothing to do; the key event should be processed normally.
    None,
    /// Transition Active → Locked. The caller should render the
    /// lockscreen, power down peripherals, and enter light sleep.
    EnterLock,
    /// Transition Locked → Active. The caller should power the
    /// display back on and re-render the terminal. The key event
    /// that caused the unlock is consumed (not forwarded to the
    /// terminal).
    ExitLock,
}

pub struct Power {
    state: PowerState,
}

impl Power {
    pub fn new() -> Self {
        Power { state: PowerState::Active }
    }

    pub fn state(&self) -> PowerState {
        self.state
    }

    pub fn is_locked(&self) -> bool {
        matches!(self.state, PowerState::Locked)
    }

    /// Process an incoming key event and return the side-effect the
    /// caller should perform.
    pub fn handle_key(&mut self, event: KeyEvent) -> PowerAction {
        match (self.state, event) {
            (PowerState::Active, KeyEvent::Lock) => {
                self.state = PowerState::Locked;
                PowerAction::EnterLock
            }
            (PowerState::Active, _) => PowerAction::None,
            (PowerState::Locked, KeyEvent::Lock) => {
                self.state = PowerState::Active;
                PowerAction::ExitLock
            }
            (PowerState::Locked, _) => {
                // Any other key while locked is ignored. The caller
                // should drain it and re-enter light sleep.
                PowerAction::None
            }
        }
    }
}

impl Default for Power {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_active() {
        let p = Power::new();
        assert_eq!(p.state(), PowerState::Active);
        assert!(!p.is_locked());
    }

    #[test]
    fn alt_l_locks() {
        let mut p = Power::new();
        let action = p.handle_key(KeyEvent::Lock);
        assert_eq!(action, PowerAction::EnterLock);
        assert!(p.is_locked());
    }

    #[test]
    fn other_keys_in_active_do_nothing() {
        let mut p = Power::new();
        assert_eq!(p.handle_key(KeyEvent::Char('a')), PowerAction::None);
        assert_eq!(p.handle_key(KeyEvent::Enter), PowerAction::None);
        assert_eq!(p.handle_key(KeyEvent::Backspace), PowerAction::None);
        assert!(!p.is_locked());
    }

    #[test]
    fn lock_event_in_locked_state_unlocks() {
        let mut p = Power::new();
        p.handle_key(KeyEvent::Lock);
        assert!(p.is_locked());

        let action = p.handle_key(KeyEvent::Lock);
        assert_eq!(action, PowerAction::ExitLock);
        assert!(!p.is_locked());
    }

    #[test]
    fn non_lock_keys_in_locked_state_are_ignored() {
        // The state must stay Locked for any non-Lock event.
        let cases = [
            KeyEvent::Char('a'),
            KeyEvent::Char(' '),
            KeyEvent::Enter,
            KeyEvent::Backspace,
            KeyEvent::SoftEnter,
            KeyEvent::Cancel,
            KeyEvent::ScrollUp,
            KeyEvent::ScrollDown,
            KeyEvent::ScrollBottom,
        ];
        for event in cases {
            let mut p = Power::new();
            p.handle_key(KeyEvent::Lock); // lock
            let action = p.handle_key(event);
            assert_eq!(
                action,
                PowerAction::None,
                "{:?} should be ignored while locked",
                event
            );
            assert!(p.is_locked(), "{:?} must not unlock", event);
        }
    }

    #[test]
    fn lock_unlock_lock_cycles() {
        let mut p = Power::new();
        assert_eq!(p.handle_key(KeyEvent::Lock), PowerAction::EnterLock);
        assert_eq!(p.handle_key(KeyEvent::Lock), PowerAction::ExitLock);
        assert_eq!(p.handle_key(KeyEvent::Lock), PowerAction::EnterLock);
        assert!(p.is_locked());
    }
}
