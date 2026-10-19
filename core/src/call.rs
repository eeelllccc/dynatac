//! In-call controls for a voice call: earpiece volume and key mapping.
//!
//! Volume is the A7682E's `AT+COUTGAIN` output gain, 0..=7. At the modem
//! default (4) the remote party is barely audible, so calls start at `MAX`.
//! `AT+CLVL` is not supported on this firmware and is not used.
//!
//! Pure logic — the caller sends the AT commands and draws the strings.
//!
//! Invariant: a `Volume` always holds a level in `0..=MAX`.

use crate::keymap::KeyEvent;

/// Highest output gain the modem accepts.
pub const MAX: u8 = 7;

/// AT command that reads back the current output gain.
pub const QUERY_COMMAND: &str = "AT+COUTGAIN?";

/// One-line help shown when a call starts.
pub const HELP: &str = "u/d: volume  Enter: hang up";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume(u8);

impl Volume {
    pub const fn max() -> Self {
        Volume(MAX)
    }

    /// `None` if `level > MAX`.
    pub fn from_level(level: u8) -> Option<Self> {
        (level <= MAX).then_some(Volume(level))
    }

    pub fn level(self) -> u8 {
        self.0
    }

    /// One step louder, saturating at `MAX`.
    pub fn up(self) -> Self {
        Volume((self.0 + 1).min(MAX))
    }

    /// One step quieter, saturating at 0.
    pub fn down(self) -> Self {
        Volume(self.0.saturating_sub(1))
    }

    /// AT command that sets the modem to this volume.
    pub fn set_command(self) -> String {
        format!("AT+COUTGAIN={}", self.0)
    }
}

/// Parse the response lines of `QUERY_COMMAND`, e.g. `["+COUTGAIN: 7"]`.
/// Returns `None` if no line carries a valid level.
pub fn parse_query(lines: &[String]) -> Option<Volume> {
    lines
        .iter()
        .find_map(|l| l.trim().strip_prefix("+COUTGAIN:"))
        .and_then(|rest| rest.trim().parse().ok())
        .and_then(Volume::from_level)
}

/// What a key press means during a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKey {
    VolumeUp,
    VolumeDown,
    HangUp,
    Ignore,
}

pub fn classify_key(event: &KeyEvent) -> CallKey {
    match event {
        KeyEvent::Char('u') => CallKey::VolumeUp,
        KeyEvent::Char('d') => CallKey::VolumeDown,
        KeyEvent::Enter => CallKey::HangUp,
        _ => CallKey::Ignore,
    }
}

/// Status line for the screen, e.g. `vol 7 (max=7)`.
///
/// `confirmed` is whether the modem read back this level. If not, a `?`
/// marks the level as what we asked for rather than what the modem reports.
pub fn status_line(volume: Volume, confirmed: bool) -> String {
    let mark = if confirmed { "" } else { "?" };
    format!("vol {}{} (max={})", volume.level(), mark, MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn starts_at_max() {
        assert_eq!(Volume::max().level(), 7);
    }

    #[test]
    fn up_saturates_at_max() {
        assert_eq!(Volume::max().up(), Volume::max());
    }

    #[test]
    fn down_saturates_at_zero() {
        let zero = Volume::from_level(0).unwrap();
        assert_eq!(zero.down(), zero);
    }

    #[test]
    fn down_then_up_round_trips() {
        assert_eq!(Volume::max().down().level(), 6);
        assert_eq!(Volume::max().down().up(), Volume::max());
    }

    #[test]
    fn from_level_rejects_above_max() {
        assert_eq!(Volume::from_level(8), None);
        assert!(Volume::from_level(7).is_some());
    }

    #[test]
    fn set_command_formats_level() {
        assert_eq!(Volume::max().set_command(), "AT+COUTGAIN=7");
        assert_eq!(Volume::max().down().set_command(), "AT+COUTGAIN=6");
    }

    #[test]
    fn parse_query_reads_level() {
        assert_eq!(parse_query(&lines(&["+COUTGAIN: 7"])), Some(Volume::max()));
        assert_eq!(parse_query(&lines(&["+COUTGAIN:3"])), Volume::from_level(3));
    }

    #[test]
    fn parse_query_rejects_garbage_and_out_of_range() {
        assert_eq!(parse_query(&lines(&[])), None);
        assert_eq!(parse_query(&lines(&["+COUTGAIN: x"])), None);
        assert_eq!(parse_query(&lines(&["+COUTGAIN: 9"])), None);
        assert_eq!(parse_query(&lines(&["+CLVL: 3"])), None);
    }

    #[test]
    fn keys_map_to_controls() {
        assert_eq!(classify_key(&KeyEvent::Char('u')), CallKey::VolumeUp);
        assert_eq!(classify_key(&KeyEvent::Char('d')), CallKey::VolumeDown);
        assert_eq!(classify_key(&KeyEvent::Enter), CallKey::HangUp);
        assert_eq!(classify_key(&KeyEvent::Char('x')), CallKey::Ignore);
        assert_eq!(classify_key(&KeyEvent::Backspace), CallKey::Ignore);
    }

    #[test]
    fn status_line_shows_level_and_max() {
        assert_eq!(status_line(Volume::max(), true), "vol 7 (max=7)");
        assert_eq!(status_line(Volume::max().down(), false), "vol 6? (max=7)");
    }

    #[test]
    fn help_fits_terminal_width() {
        assert!(HELP.len() <= crate::terminal::COLS);
        assert!(status_line(Volume::max(), false).len() <= crate::terminal::COLS);
    }
}
