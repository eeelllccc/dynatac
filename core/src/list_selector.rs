//! Reusable interactive list selector.
//!
//! Presents a list of items with a cursor. The user navigates with
//! `y` (up) and `h` (down), and confirms with Enter.
//!
//! Caller invariants:
//!   - Feed key events via `handle_key`
//!   - On `ListAction::Redraw`, call `render()` and display the result
//!   - On `ListAction::Selected(item)`, the interaction is complete

use crate::keymap::KeyEvent;

/// What happened after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListAction {
    /// Cursor moved; caller should re-render.
    Redraw,
    /// User confirmed selection.
    Selected(String),
    /// Key was irrelevant; nothing changed.
    None,
}

pub struct ListSelector {
    header: String,
    items: Vec<String>,
    cursor: usize,
}

impl ListSelector {
    /// Create a new selector. `items` must not be empty.
    pub fn new(header: &str, items: Vec<String>) -> Self {
        assert!(!items.is_empty(), "ListSelector requires at least one item");
        Self {
            header: header.to_string(),
            items,
            cursor: 0,
        }
    }

    /// Process a key event.
    /// - `y` moves cursor up
    /// - `h` moves cursor down
    /// - Enter selects the current item
    pub fn handle_key(&mut self, event: KeyEvent) -> ListAction {
        match event {
            KeyEvent::Char('y') => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    ListAction::Redraw
                } else {
                    ListAction::None
                }
            }
            KeyEvent::Char('h') => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                    ListAction::Redraw
                } else {
                    ListAction::None
                }
            }
            KeyEvent::Enter => {
                ListAction::Selected(self.items[self.cursor].clone())
            }
            _ => ListAction::None,
        }
    }

    /// Render the list as a multi-line string.
    /// The selected item is marked with `> `, others with `  `.
    pub fn render(&self) -> String {
        let mut lines = Vec::with_capacity(self.items.len() + 1);
        lines.push(self.header.clone());
        for (i, item) in self.items.iter().enumerate() {
            let marker = if i == self.cursor { "> " } else { "  " };
            lines.push(format!("{}{}", marker, item));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> ListSelector {
        ListSelector::new(
            "Pick one:",
            vec!["alpha".into(), "beta".into(), "gamma".into()],
        )
    }

    #[test]
    fn initial_cursor_at_top() {
        let sel = selector();
        let rendered = sel.render();
        assert!(rendered.contains("> alpha"));
        assert!(rendered.contains("  beta"));
        assert!(rendered.contains("  gamma"));
    }

    #[test]
    fn h_moves_cursor_down() {
        let mut sel = selector();
        assert_eq!(sel.handle_key(KeyEvent::Char('h')), ListAction::Redraw);
        let rendered = sel.render();
        assert!(rendered.contains("  alpha"));
        assert!(rendered.contains("> beta"));
    }

    #[test]
    fn y_moves_cursor_up() {
        let mut sel = selector();
        sel.handle_key(KeyEvent::Char('h')); // move to beta
        assert_eq!(sel.handle_key(KeyEvent::Char('y')), ListAction::Redraw);
        let rendered = sel.render();
        assert!(rendered.contains("> alpha"));
    }

    #[test]
    fn y_at_top_is_none() {
        let mut sel = selector();
        assert_eq!(sel.handle_key(KeyEvent::Char('y')), ListAction::None);
    }

    #[test]
    fn h_at_bottom_is_none() {
        let mut sel = selector();
        sel.handle_key(KeyEvent::Char('h'));
        sel.handle_key(KeyEvent::Char('h')); // at gamma
        assert_eq!(sel.handle_key(KeyEvent::Char('h')), ListAction::None);
    }

    #[test]
    fn enter_selects_current_item() {
        let mut sel = selector();
        sel.handle_key(KeyEvent::Char('h')); // move to beta
        assert_eq!(
            sel.handle_key(KeyEvent::Enter),
            ListAction::Selected("beta".into())
        );
    }

    #[test]
    fn enter_selects_first_item_by_default() {
        let mut sel = selector();
        assert_eq!(
            sel.handle_key(KeyEvent::Enter),
            ListAction::Selected("alpha".into())
        );
    }

    #[test]
    fn irrelevant_keys_return_none() {
        let mut sel = selector();
        assert_eq!(sel.handle_key(KeyEvent::Char('x')), ListAction::None);
        assert_eq!(sel.handle_key(KeyEvent::Backspace), ListAction::None);
    }

    #[test]
    fn render_includes_header() {
        let sel = selector();
        let rendered = sel.render();
        assert!(rendered.starts_with("Pick one:"));
    }

    #[test]
    #[should_panic(expected = "at least one item")]
    fn empty_items_panics() {
        ListSelector::new("header", vec![]);
    }
}
