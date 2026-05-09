//! Reusable interactive list selector.
//!
//! Presents a list of items with a cursor. The user navigates with
//! `y` (up) and `h` (down), and confirms with Enter.
//!
//! The selector is display-aware: it only renders items that fit within
//! `visible_rows`. The viewport scrolls automatically to keep the cursor
//! on screen. Call `set_visible_rows` when the font size changes.
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
    /// Display strings shown on screen.
    items: Vec<String>,
    /// Values returned by `ListAction::Selected`. Always the same length as
    /// `items`. When constructed via `new`, values == items.
    values: Vec<String>,
    cursor: usize,
    /// Total display rows available (1 for header + rest for items).
    visible_rows: usize,
    /// Index of the first item row currently shown.
    viewport_offset: usize,
}

impl ListSelector {
    /// Create a new selector.
    ///
    /// `visible_rows` is the total number of display rows available to this
    /// widget (one row is reserved for the header). Pass the current value of
    /// `Shell::display_rows` so the selector stays in sync with the font size.
    ///
    /// `items` must not be empty.
    /// Create a selector where the displayed text and selection value are the
    /// same string (the common case for SSIDs, commands, etc.).
    pub fn new(header: &str, items: Vec<String>, visible_rows: usize) -> Self {
        assert!(!items.is_empty(), "ListSelector requires at least one item");
        let values = items.clone();
        Self {
            header: header.to_string(),
            items,
            values,
            cursor: 0,
            visible_rows,
            viewport_offset: 0,
        }
    }

    /// Create a selector where the displayed text and the value returned on
    /// selection are different. Each tuple is `(display, value)`.
    ///
    /// Use this when the display string needs to be short (to avoid wrapping)
    /// but the selection value needs to carry more information (e.g. a full
    /// JID or path).
    pub fn new_with_values(
        header: &str,
        items: Vec<(String, String)>,
        visible_rows: usize,
    ) -> Self {
        assert!(!items.is_empty(), "ListSelector requires at least one item");
        let (displays, values): (Vec<_>, Vec<_>) = items.into_iter().unzip();
        Self {
            header: header.to_string(),
            items: displays,
            values,
            cursor: 0,
            visible_rows,
            viewport_offset: 0,
        }
    }

    /// Update the available display rows after a font-size change.
    /// The viewport is adjusted so the cursor remains visible.
    pub fn set_visible_rows(&mut self, rows: usize) {
        self.visible_rows = rows;
        self.clamp_viewport();
    }

    /// Number of item rows the viewport can show.
    fn item_rows(&self) -> usize {
        self.visible_rows.saturating_sub(1)
    }

    /// Scroll the viewport so the cursor is always within it.
    fn clamp_viewport(&mut self) {
        let n = self.item_rows();
        if n == 0 {
            self.viewport_offset = 0;
            return;
        }
        if self.cursor < self.viewport_offset {
            self.viewport_offset = self.cursor;
        } else if self.cursor >= self.viewport_offset + n {
            self.viewport_offset = self.cursor + 1 - n;
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
                    self.clamp_viewport();
                    ListAction::Redraw
                } else {
                    ListAction::None
                }
            }
            KeyEvent::Char('h') => {
                if self.cursor + 1 < self.items.len() {
                    self.cursor += 1;
                    self.clamp_viewport();
                    ListAction::Redraw
                } else {
                    ListAction::None
                }
            }
            KeyEvent::Enter => ListAction::Selected(self.values[self.cursor].clone()),
            _ => ListAction::None,
        }
    }

    /// Render the visible portion of the list.
    /// The selected item is marked with `> `, others with `  `.
    pub fn render(&self) -> String {
        let n = self.item_rows();
        let end = (self.viewport_offset + n).min(self.items.len());
        let mut lines = Vec::with_capacity(end - self.viewport_offset + 1);
        lines.push(self.header.clone());
        for i in self.viewport_offset..end {
            let marker = if i == self.cursor { "> " } else { "  " };
            lines.push(format!("{}{}", marker, self.items[i]));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tall enough that all 3 items always fit on screen.
    fn selector() -> ListSelector {
        ListSelector::new(
            "Pick one:",
            vec!["alpha".into(), "beta".into(), "gamma".into()],
            10,
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
        ListSelector::new("header", vec![], 10);
    }

    // --- Viewport / scrolling tests ---

    /// 3 items, only 2 rows for items (visible_rows=3: 1 header + 2 items).
    fn tight_selector() -> ListSelector {
        ListSelector::new(
            "header:",
            vec!["a".into(), "b".into(), "c".into()],
            3, // 1 header row + 2 item rows
        )
    }

    #[test]
    fn viewport_shows_only_visible_rows() {
        let sel = tight_selector();
        let rendered = sel.render();
        // Initial viewport: items 0..2 (a, b). c is out of view.
        assert!(rendered.contains("> a"));
        assert!(rendered.contains("  b"));
        assert!(!rendered.contains("c"));
    }

    #[test]
    fn viewport_scrolls_down_when_cursor_leaves() {
        let mut sel = tight_selector();
        sel.handle_key(KeyEvent::Char('h')); // cursor → b
        sel.handle_key(KeyEvent::Char('h')); // cursor → c; viewport scrolls
        let rendered = sel.render();
        // Viewport now shows b, c. a is scrolled out.
        assert!(!rendered.contains("  a"), "item a should not be visible");
        assert!(!rendered.contains("> a"), "item a should not be visible");
        assert!(rendered.contains("  b"));
        assert!(rendered.contains("> c"));
    }

    #[test]
    fn viewport_scrolls_back_up() {
        let mut sel = tight_selector();
        sel.handle_key(KeyEvent::Char('h'));
        sel.handle_key(KeyEvent::Char('h')); // scroll down to show b, c
        sel.handle_key(KeyEvent::Char('y')); // cursor → b
        sel.handle_key(KeyEvent::Char('y')); // cursor → a; viewport scrolls back
        let rendered = sel.render();
        assert!(rendered.contains("> a"));
        assert!(rendered.contains("  b"));
        assert!(!rendered.contains("  c"), "item c should not be visible");
        assert!(!rendered.contains("> c"), "item c should not be visible");
    }

    #[test]
    fn set_visible_rows_adjusts_viewport_to_keep_cursor_visible() {
        let mut sel = tight_selector();
        sel.handle_key(KeyEvent::Char('h'));
        sel.handle_key(KeyEvent::Char('h')); // cursor at c, viewport at b..c

        // Shrink to 2 rows (1 header + 1 item). Viewport must still show cursor.
        sel.set_visible_rows(2);
        let rendered = sel.render();
        assert!(rendered.contains("> c"));
        assert!(!rendered.contains("b"));
    }

    #[test]
    fn set_visible_rows_expand_shows_more_items() {
        let mut sel = tight_selector();
        // Expand to show all 3 items.
        sel.set_visible_rows(10);
        let rendered = sel.render();
        assert!(rendered.contains("> a"));
        assert!(rendered.contains("  b"));
        assert!(rendered.contains("  c"));
    }
}
