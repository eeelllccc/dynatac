//! Reusable interactive text prompt.
//!
//! Presents a header and collects a single line of text input.
//! Characters are masked with `*` (suitable for passwords).
//!
//! Caller invariants:
//!   - Feed key events via `handle_key`
//!   - On `TextPromptAction::Redraw`, call `render()` and display the result
//!   - On `TextPromptAction::Submitted(text)`, the interaction is complete

use crate::keymap::KeyEvent;

/// What happened after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPromptAction {
    /// Input changed; caller should re-render.
    Redraw,
    /// User pressed Enter; contains the submitted text.
    Submitted(String),
    /// Key was irrelevant; nothing changed.
    None,
}

pub struct TextPrompt {
    header: String,
    input: String,
}

impl TextPrompt {
    pub fn new(header: &str) -> Self {
        Self {
            header: header.to_string(),
            input: String::new(),
        }
    }

    /// Process a key event.
    /// - Printable chars are appended
    /// - Backspace removes the last char
    /// - Enter submits the input
    pub fn handle_key(&mut self, event: KeyEvent) -> TextPromptAction {
        match event {
            KeyEvent::Char(ch) => {
                self.input.push(ch);
                TextPromptAction::Redraw
            }
            KeyEvent::Backspace => {
                if self.input.pop().is_some() {
                    TextPromptAction::Redraw
                } else {
                    TextPromptAction::None
                }
            }
            KeyEvent::Enter => TextPromptAction::Submitted(self.input.clone()),
            KeyEvent::ScrollUp | KeyEvent::ScrollDown | KeyEvent::ScrollBottom => {
                TextPromptAction::None
            }
        }
    }

    /// Render the prompt as a multi-line string.
    /// Input is masked with `*` characters.
    pub fn render(&self) -> String {
        let masked: String = "*".repeat(self.input.len());
        format!("{}\n> {}", self.header, masked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_render_shows_header_and_empty_input() {
        let prompt = TextPrompt::new("password:");
        assert_eq!(prompt.render(), "password:\n> ");
    }

    #[test]
    fn typing_chars_masks_with_stars() {
        let mut prompt = TextPrompt::new("password:");
        assert_eq!(prompt.handle_key(KeyEvent::Char('a')), TextPromptAction::Redraw);
        assert_eq!(prompt.handle_key(KeyEvent::Char('b')), TextPromptAction::Redraw);
        assert_eq!(prompt.render(), "password:\n> **");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut prompt = TextPrompt::new("password:");
        prompt.handle_key(KeyEvent::Char('a'));
        prompt.handle_key(KeyEvent::Char('b'));
        assert_eq!(prompt.handle_key(KeyEvent::Backspace), TextPromptAction::Redraw);
        assert_eq!(prompt.render(), "password:\n> *");
    }

    #[test]
    fn backspace_on_empty_is_none() {
        let mut prompt = TextPrompt::new("password:");
        assert_eq!(prompt.handle_key(KeyEvent::Backspace), TextPromptAction::None);
    }

    #[test]
    fn enter_submits_input() {
        let mut prompt = TextPrompt::new("password:");
        prompt.handle_key(KeyEvent::Char('s'));
        prompt.handle_key(KeyEvent::Char('e'));
        prompt.handle_key(KeyEvent::Char('c'));
        assert_eq!(
            prompt.handle_key(KeyEvent::Enter),
            TextPromptAction::Submitted("sec".into())
        );
    }

    #[test]
    fn enter_on_empty_submits_empty_string() {
        let mut prompt = TextPrompt::new("password:");
        assert_eq!(
            prompt.handle_key(KeyEvent::Enter),
            TextPromptAction::Submitted(String::new())
        );
    }

}
