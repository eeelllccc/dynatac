//! Reusable read-only scrollable view (`less`-style pager).
//!
//! Displays a fixed set of lines with a scrollable viewport. Unlike
//! `ListSelector` the user cannot select items; they can only scroll and quit.
//!
//! Keys:
//!   - `y`                    → scroll up one line
//!   - `h`                    → scroll down one line
//!   - `q` or `Cancel`        → exit (returns `ScrollAction::Exit`)
//!
//! Caller invariants:
//!   - On `ScrollAction::Redraw`, call `render()` and display the result.
//!   - On `ScrollAction::Exit`, tear down the view and return to normal mode.
//!   - Call `set_visible_rows` when the font size changes.

use crate::keymap::KeyEvent;

/// Where to position the viewport when the view is first shown.
pub enum ScrollStart {
    Top,
    Bottom,
}

/// What happened after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrollAction {
    /// Viewport moved; caller should re-render.
    Redraw,
    /// User quit; caller should leave scroll mode.
    Exit,
    /// Key was irrelevant; nothing changed.
    None,
}

pub struct ScrollView {
    lines: Vec<String>,
    visible_rows: usize,
    offset: usize,
}

impl ScrollView {
    pub fn new(lines: Vec<String>, visible_rows: usize, start: ScrollStart) -> Self {
        let offset = match start {
            ScrollStart::Top => 0,
            ScrollStart::Bottom => lines.len().saturating_sub(visible_rows),
        };
        Self { lines, visible_rows, offset }
    }

    pub fn set_visible_rows(&mut self, rows: usize) {
        self.visible_rows = rows;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ScrollAction {
        match key {
            KeyEvent::Char('y') => {
                if self.offset > 0 {
                    self.offset -= 1;
                    ScrollAction::Redraw
                } else {
                    ScrollAction::None
                }
            }
            KeyEvent::Char('h') => {
                if self.offset + self.visible_rows < self.lines.len() {
                    self.offset += 1;
                    ScrollAction::Redraw
                } else {
                    ScrollAction::None
                }
            }
            KeyEvent::Char('q') | KeyEvent::Cancel => ScrollAction::Exit,
            _ => ScrollAction::None,
        }
    }

    pub fn render(&self) -> String {
        let end = (self.offset + self.visible_rows).min(self.lines.len());
        self.lines[self.offset..end].join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(n: usize) -> ScrollView {
        let lines = (1..=5).map(|i| format!("line{i}")).collect();
        ScrollView::new(lines, n, ScrollStart::Top)
    }

    #[test]
    fn top_start_renders_from_beginning() {
        let v = view(10);
        let r = v.render();
        assert!(r.starts_with("line1"));
        assert!(r.contains("line5"));
    }

    #[test]
    fn bottom_start_positions_at_end() {
        let lines = (1..=5).map(|i| format!("line{i}")).collect();
        let v = ScrollView::new(lines, 3, ScrollStart::Bottom);
        let r = v.render();
        assert!(r.contains("line3"));
        assert!(r.contains("line5"));
        assert!(!r.contains("line1"));
        assert!(!r.contains("line2"));
    }

    #[test]
    fn h_scrolls_down() {
        let mut v = view(3);
        assert_eq!(v.handle_key(KeyEvent::Char('h')), ScrollAction::Redraw);
        let r = v.render();
        assert!(!r.contains("line1"), "line1 should scroll off");
        assert!(r.contains("line2"));
        assert!(r.contains("line4"));
    }

    #[test]
    fn y_scrolls_back_up() {
        let mut v = view(3);
        v.handle_key(KeyEvent::Char('h'));
        assert_eq!(v.handle_key(KeyEvent::Char('y')), ScrollAction::Redraw);
        let r = v.render();
        assert!(r.starts_with("line1"));
    }

    #[test]
    fn y_at_top_is_none() {
        let mut v = view(3);
        assert_eq!(v.handle_key(KeyEvent::Char('y')), ScrollAction::None);
    }

    #[test]
    fn h_at_bottom_is_none() {
        let mut v = view(10); // all 5 lines fit
        assert_eq!(v.handle_key(KeyEvent::Char('h')), ScrollAction::None);
    }

    #[test]
    fn q_exits() {
        let mut v = view(3);
        assert_eq!(v.handle_key(KeyEvent::Char('q')), ScrollAction::Exit);
    }

    #[test]
    fn cancel_exits() {
        let mut v = view(3);
        assert_eq!(v.handle_key(KeyEvent::Cancel), ScrollAction::Exit);
    }

    #[test]
    fn irrelevant_key_is_none() {
        let mut v = view(3);
        assert_eq!(v.handle_key(KeyEvent::Char('x')), ScrollAction::None);
        assert_eq!(v.handle_key(KeyEvent::Enter), ScrollAction::None);
    }

    #[test]
    fn empty_view_renders_empty_string() {
        let v = ScrollView::new(vec![], 5, ScrollStart::Top);
        assert_eq!(v.render(), "");
    }
}
