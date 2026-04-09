// Reusable terminal interface for the dynatac text OS.
//
// Manages a text grid of configurable size with:
//   - Scrollback buffer of output lines
//   - Input line with cursor, wrapping across rows
//   - Prompt display
//
// Pure logic — no hardware deps. The caller maps RenderCells to
// FrameBuffer::draw_char calls.
//
// Caller invariants:
//   - Call `handle_key` for each KeyEvent from the keyboard
//   - On `TerminalAction::Execute(cmd)`, run the command and call `push_output`
//   - On `Redraw` or after `push_output`, call `render()` and draw to framebuffer

use crate::keymap::KeyEvent;

pub const COLS: usize = 30;
pub const ROWS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCell {
    pub col: u8,
    pub row: u8,
    pub ch: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAction {
    /// Screen content changed; caller should render + flush.
    Redraw,
    /// User pressed Enter; caller should execute the command, then push_output.
    Execute(String),
    /// Nothing visible changed.
    None,
}

pub struct Terminal {
    cols: usize,
    rows: usize,
    lines: Vec<String>,
    input: String,
    cursor: usize,
    prompt: String,
    show_input: bool,
    scroll_offset: usize,
}

impl Terminal {
    pub fn new(prompt: &str, cols: usize, rows: usize) -> Self {
        Self {
            cols,
            rows,
            lines: Vec::new(),
            input: String::new(),
            cursor: 0,
            prompt: prompt.to_string(),
            show_input: true,
            scroll_offset: 0,
        }
    }

    /// Clear the scrollback buffer, keeping the input line intact.
    pub fn clear(&mut self) {
        self.lines.clear();
    }

    /// Show or hide the input line (prompt + cursor).
    /// When hidden, all rows are used for output.
    pub fn set_show_input(&mut self, show: bool) {
        self.show_input = show;
    }

    /// Feed a key event into the terminal.
    pub fn handle_key(&mut self, event: KeyEvent) -> TerminalAction {
        match event {
            KeyEvent::Char(ch) => {
                self.input.insert(self.cursor, ch);
                self.cursor += 1;
                TerminalAction::Redraw
            }
            KeyEvent::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                    TerminalAction::Redraw
                } else {
                    TerminalAction::None
                }
            }
            KeyEvent::Enter => {
                let cmd = self.input.clone();
                // Record the prompt+command in scrollback
                let submitted = format!("{}{}", self.prompt, cmd);
                self.lines.push(submitted);
                self.input.clear();
                self.cursor = 0;
                self.scroll_offset = 0;
                TerminalAction::Execute(cmd)
            }
            KeyEvent::ScrollUp => {
                let max = self.max_scroll_offset();
                if self.scroll_offset < max {
                    self.scroll_offset += 1;
                }
                TerminalAction::Redraw
            }
            KeyEvent::ScrollDown => {
                if self.scroll_offset > 0 {
                    self.scroll_offset -= 1;
                }
                TerminalAction::Redraw
            }
            KeyEvent::ScrollBottom => {
                self.scroll_offset = 0;
                TerminalAction::Redraw
            }
        }
    }

    /// Add output text to the scrollback buffer.
    /// Splits on newlines so each line is stored separately.
    /// Resets scroll to the bottom so the newest content is visible.
    pub fn push_output(&mut self, text: &str) {
        for line in text.split('\n') {
            self.lines.push(line.to_string());
        }
        self.scroll_offset = 0;
    }

    /// Wrap all scrollback lines to the terminal width.
    fn wrapped_output(&self) -> Vec<String> {
        let mut output_wrapped: Vec<String> = Vec::new();
        for line in &self.lines {
            if line.is_empty() {
                output_wrapped.push(String::new());
            } else {
                for chunk in wrap(line, self.cols) {
                    output_wrapped.push(chunk);
                }
            }
        }
        output_wrapped
    }

    /// How many rows the input line occupies.
    fn input_row_count(&self) -> usize {
        if self.show_input {
            let input_display = format!("{}{}_", self.prompt, self.input);
            wrap(&input_display, self.cols).len().min(self.rows)
        } else {
            0
        }
    }

    /// Maximum scroll offset (how far up from the bottom you can scroll).
    fn max_scroll_offset(&self) -> usize {
        let rows_for_output = self.rows.saturating_sub(self.input_row_count());
        let total_output = self.wrapped_output().len();
        total_output.saturating_sub(rows_for_output)
    }

    /// Render the visible terminal state as a list of cells to draw.
    ///
    /// The screen is filled bottom-up: the input line (with prompt, text, and
    /// cursor) occupies the bottom rows, and scrollback output fills above it.
    pub fn render(&self) -> Vec<RenderCell> {
        let mut cells = Vec::new();
        let cols = self.cols;
        let rows = self.rows;

        // When input is hidden, all rows go to output.
        let (input_wrapped, input_row_count) = if self.show_input {
            let input_display = format!("{}{}_", self.prompt, self.input);
            let wrapped = wrap(&input_display, cols);
            let count = wrapped.len().min(rows);
            (wrapped, count)
        } else {
            (Vec::new(), 0)
        };
        let rows_for_output = rows.saturating_sub(input_row_count);

        let output_wrapped = self.wrapped_output();

        // Apply scroll offset: show a window ending at (len - scroll_offset)
        let end = output_wrapped.len().saturating_sub(self.scroll_offset);
        let start = end.saturating_sub(rows_for_output);
        let visible_output = &output_wrapped[start..end];

        // Place output rows starting from the top of their visible area
        let output_start_row = rows_for_output - visible_output.len();
        for (i, row_text) in visible_output.iter().enumerate() {
            let screen_row = (output_start_row + i) as u8;
            for (col, ch) in row_text.chars().enumerate() {
                cells.push(RenderCell {
                    col: col as u8,
                    row: screen_row,
                    ch,
                });
            }
        }

        // Place input rows at the bottom (take only the last `input_row_count`)
        let input_visible = if input_wrapped.len() > input_row_count {
            &input_wrapped[input_wrapped.len() - input_row_count..]
        } else {
            &input_wrapped[..]
        };
        for (i, row_text) in input_visible.iter().enumerate() {
            let screen_row = (rows - input_row_count + i) as u8;
            for (col, ch) in row_text.chars().enumerate() {
                cells.push(RenderCell {
                    col: col as u8,
                    row: screen_row,
                    ch,
                });
            }
        }

        cells
    }
}

/// Wrap a string into chunks of at most `cols` characters.
fn wrap(s: &str, cols: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut remaining = s;
    while remaining.len() > cols {
        rows.push(remaining[..cols].to_string());
        remaining = &remaining[cols..];
    }
    rows.push(remaining.to_string());
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeyEvent;

    /// Helper: find the cell at (col, row) in the render output.
    fn cell_at(cells: &[RenderCell], col: u8, row: u8) -> Option<&RenderCell> {
        cells.iter().find(|c| c.col == col && c.row == row)
    }

    /// Helper: collect all chars on a given row, in column order.
    fn row_text(cells: &[RenderCell], row: u8) -> String {
        let mut row_cells: Vec<&RenderCell> = cells.iter().filter(|c| c.row == row).collect();
        row_cells.sort_by_key(|c| c.col);
        // Deduplicate: if cursor overlaps a char, keep the last one written
        let mut result = String::new();
        let mut last_col: Option<u8> = None;
        for c in &row_cells {
            if last_col == Some(c.col) {
                result.pop();
            }
            result.push(c.ch);
            last_col = Some(c.col);
        }
        result
    }

    const R: usize = ROWS; // bottom row index = R - 1

    fn term() -> Terminal {
        Terminal::new("> ", COLS, ROWS)
    }

    // Step 1: Empty terminal renders prompt + cursor
    #[test]
    fn empty_terminal_renders_prompt_and_cursor() {
        let term = term();
        let cells = term.render();

        assert_eq!(cell_at(&cells, 0, (R - 1) as u8).unwrap().ch, '>');
        assert_eq!(cell_at(&cells, 1, (R - 1) as u8).unwrap().ch, ' ');
        assert_eq!(cell_at(&cells, 2, (R - 1) as u8).unwrap().ch, '_');
    }

    // Step 2: Typing characters
    #[test]
    fn typing_characters_appear_after_prompt() {
        let mut term = term();
        assert_eq!(term.handle_key(KeyEvent::Char('h')), TerminalAction::Redraw);
        assert_eq!(term.handle_key(KeyEvent::Char('i')), TerminalAction::Redraw);

        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 1) as u8), "> hi_");
    }

    // Step 3: Cursor position
    #[test]
    fn cursor_follows_typed_text() {
        let mut term = term();
        term.handle_key(KeyEvent::Char('h'));
        term.handle_key(KeyEvent::Char('i'));

        let cells = term.render();
        assert_eq!(cell_at(&cells, 4, (R - 1) as u8).unwrap().ch, '_');
    }

    // Step 4: Backspace
    #[test]
    fn backspace_removes_last_char() {
        let mut term = term();
        term.handle_key(KeyEvent::Char('h'));
        term.handle_key(KeyEvent::Char('i'));
        assert_eq!(term.handle_key(KeyEvent::Backspace), TerminalAction::Redraw);

        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 1) as u8), "> h_");
    }

    #[test]
    fn backspace_on_empty_input_is_none() {
        let mut term = term();
        assert_eq!(term.handle_key(KeyEvent::Backspace), TerminalAction::None);
    }

    // Step 5: Input wrapping
    #[test]
    fn long_input_wraps_to_row_above() {
        let mut term = term();
        // Prompt is 2 chars, so 27 chars + cursor = 30 fits one row.
        // Type 28 chars: "> " + 28 + "_" = 31 chars → wraps to 2 rows.
        for ch in "abcdefghijklmnopqrstuvwxyzAB".chars() {
            term.handle_key(KeyEvent::Char(ch));
        }
        let cells = term.render();
        // "> abcdef...AB_" = 31 chars → row R-2 has 30, row R-1 has "_"
        assert_eq!(row_text(&cells, (R - 2) as u8).len(), 30);
        assert!(row_text(&cells, (R - 2) as u8).starts_with("> "));
        assert_eq!(row_text(&cells, (R - 1) as u8), "_");

        // Type one more: "> ...ABC_" = 32 chars → row R-2 has 30, row R-1 has "BC_"
        term.handle_key(KeyEvent::Char('C'));
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8).len(), 30);
        assert_eq!(row_text(&cells, (R - 1) as u8), "C_");
    }

    // Step 5b: Cursor at exact column boundary wraps correctly
    #[test]
    fn cursor_at_col_boundary_does_not_crash() {
        let mut term = term();
        // Type exactly 28 chars: "> " + 28 + "_" = 31 → wraps, cursor on new row
        for _ in 0..28 {
            term.handle_key(KeyEvent::Char('x'));
        }
        let cells = term.render();
        // Should not panic, and cursor should be on the wrapped row
        assert_eq!(row_text(&cells, (R - 1) as u8), "_");
    }

    // Step 6: Enter executes command
    #[test]
    fn enter_returns_execute_and_clears_input() {
        let mut term = term();
        for ch in "hello".chars() {
            term.handle_key(KeyEvent::Char(ch));
        }

        let action = term.handle_key(KeyEvent::Enter);
        assert_eq!(action, TerminalAction::Execute("hello".into()));

        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 1) as u8), "> _");
    }

    // Step 7: Push output
    #[test]
    fn push_output_shows_in_scrollback() {
        let mut term = term();

        for ch in "cmd".chars() {
            term.handle_key(KeyEvent::Char(ch));
        }
        term.handle_key(KeyEvent::Enter);
        term.push_output("ok");

        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 1) as u8), "> _");
        assert_eq!(row_text(&cells, (R - 2) as u8), "ok");
        assert_eq!(row_text(&cells, (R - 3) as u8), "> cmd");
    }

    // Step 8: Output wrapping
    #[test]
    fn long_output_wraps() {
        let mut term = term();
        let long = "a".repeat(45);
        term.push_output(&long);

        let cells = term.render();
        // 45 chars → 30 on first row + 15 on second row
        assert_eq!(row_text(&cells, (R - 3) as u8).len(), 30);
        assert_eq!(row_text(&cells, (R - 2) as u8).len(), 15);
        assert_eq!(row_text(&cells, (R - 1) as u8), "> _");
    }

    // Step 9: Scrolling when output exceeds screen
    #[test]
    fn old_lines_scroll_off_top() {
        let mut term = term();
        // Push 50 short lines — more than R-1 available rows
        for i in 0..50 {
            term.push_output(&format!("line {}", i));
        }

        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 1) as u8), "> _");
        assert_eq!(row_text(&cells, (R - 2) as u8), "line 49");
        // Row 0 should be line 50-(R-1) = line 50-31 = line 19
        assert_eq!(row_text(&cells, 0), format!("line {}", 50 - (R - 1)));
    }

    // Step 10: Multi-line output
    #[test]
    fn multiline_output_splits_on_newline() {
        let mut term = term();
        term.push_output("line1\nline2\nline3");

        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 1) as u8), "> _");
        assert_eq!(row_text(&cells, (R - 2) as u8), "line3");
        assert_eq!(row_text(&cells, (R - 3) as u8), "line2");
        assert_eq!(row_text(&cells, (R - 4) as u8), "line1");
    }

    // Step 11: Hidden input line
    #[test]
    fn hidden_input_gives_all_rows_to_output() {
        let mut term = term();
        term.set_show_input(false);
        term.push_output("hello");

        let cells = term.render();
        // Output should appear on the last row (no input line taking space)
        assert_eq!(row_text(&cells, (R - 1) as u8), "hello");
        // No prompt or cursor anywhere
        let has_prompt = cells.iter().any(|c| c.ch == '>');
        assert!(!has_prompt);
        let has_cursor = cells.iter().any(|c| c.ch == '_');
        assert!(!has_cursor);
    }

    #[test]
    fn show_input_restores_prompt() {
        let mut term = term();
        term.set_show_input(false);
        term.push_output("hello");
        term.set_show_input(true);

        let cells = term.render();
        // Prompt is back on the bottom row
        assert_eq!(row_text(&cells, (R - 1) as u8), "> _");
        // Output moved up one row
        assert_eq!(row_text(&cells, (R - 2) as u8), "hello");
    }

    // --- Scroll offset tests ---

    #[test]
    fn scroll_up_shifts_viewport() {
        let mut term = term();
        for i in 0..50 {
            term.push_output(&format!("line {}", i));
        }
        // At bottom: last visible output line is "line 49"
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "line 49");

        // Scroll up one line
        assert_eq!(term.handle_key(KeyEvent::ScrollUp), TerminalAction::Redraw);
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "line 48");
    }

    #[test]
    fn scroll_down_moves_back() {
        let mut term = term();
        for i in 0..50 {
            term.push_output(&format!("line {}", i));
        }
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollDown);
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "line 48");
    }

    #[test]
    fn scroll_bottom_jumps_to_end() {
        let mut term = term();
        for i in 0..50 {
            term.push_output(&format!("line {}", i));
        }
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollBottom);
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "line 49");
    }

    #[test]
    fn scroll_offset_capped_at_max() {
        let mut term = term();
        // Push only 5 lines — less than screen height, no scrolling possible
        for i in 0..5 {
            term.push_output(&format!("line {}", i));
        }
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollUp);
        // scroll_offset should stay at 0 since all content fits
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "line 4");
    }

    #[test]
    fn push_output_resets_scroll() {
        let mut term = term();
        for i in 0..50 {
            term.push_output(&format!("line {}", i));
        }
        term.handle_key(KeyEvent::ScrollUp);
        term.handle_key(KeyEvent::ScrollUp);
        // Now push new output — should reset to bottom
        term.push_output("new line");
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "new line");
    }

    #[test]
    fn scroll_down_does_not_go_below_zero() {
        let mut term = term();
        term.push_output("hello");
        // Scroll down when already at bottom — should be a no-op
        term.handle_key(KeyEvent::ScrollDown);
        let cells = term.render();
        assert_eq!(row_text(&cells, (R - 2) as u8), "hello");
    }
}
