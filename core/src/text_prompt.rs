//! Reusable interactive text prompt.
//!
//! Presents a header and collects a single line of text input. Input is
//! either echoed verbatim ([`TextPrompt::plain`]) or masked with `*`
//! ([`TextPrompt::masked`], for passwords).
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
    mask: bool,
}

impl TextPrompt {
    /// Prompt that hides input behind `*` characters. Use for passwords.
    pub fn masked(header: &str) -> Self {
        Self {
            header: header.to_string(),
            input: String::new(),
            mask: true,
        }
    }

    /// Prompt that echoes input verbatim. Use for non-secret values like
    /// email addresses.
    pub fn plain(header: &str) -> Self {
        Self {
            header: header.to_string(),
            input: String::new(),
            mask: false,
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
            // Cancel and Lock are intercepted by the shell / main loop
            // before they ever reach the prompt, but we still need arms
            // so the match is exhaustive.
            KeyEvent::SoftEnter
            | KeyEvent::Cancel
            | KeyEvent::ScrollUp
            | KeyEvent::ScrollDown
            | KeyEvent::ScrollBottom
            | KeyEvent::Lock => TextPromptAction::None,
        }
    }

    /// Render the prompt as a multi-line string. Input is masked or
    /// echoed depending on how the prompt was constructed.
    pub fn render(&self) -> String {
        let body = if self.mask {
            "*".repeat(self.input.chars().count())
        } else {
            self.input.clone()
        };
        format!("{}\n> {}", self.header, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_render_shows_header_and_empty_input() {
        let prompt = TextPrompt::masked("password:");
        assert_eq!(prompt.render(), "password:\n> ");
    }

    #[test]
    fn masked_typing_chars_masks_with_stars() {
        let mut prompt = TextPrompt::masked("password:");
        assert_eq!(prompt.handle_key(KeyEvent::Char('a')), TextPromptAction::Redraw);
        assert_eq!(prompt.handle_key(KeyEvent::Char('b')), TextPromptAction::Redraw);
        assert_eq!(prompt.render(), "password:\n> **");
    }

    #[test]
    fn plain_typing_chars_echoes_verbatim() {
        let mut prompt = TextPrompt::plain("address:");
        prompt.handle_key(KeyEvent::Char('a'));
        prompt.handle_key(KeyEvent::Char('@'));
        prompt.handle_key(KeyEvent::Char('b'));
        assert_eq!(prompt.render(), "address:\n> a@b");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut prompt = TextPrompt::masked("password:");
        prompt.handle_key(KeyEvent::Char('a'));
        prompt.handle_key(KeyEvent::Char('b'));
        assert_eq!(prompt.handle_key(KeyEvent::Backspace), TextPromptAction::Redraw);
        assert_eq!(prompt.render(), "password:\n> *");
    }

    #[test]
    fn backspace_on_empty_is_none() {
        let mut prompt = TextPrompt::masked("password:");
        assert_eq!(prompt.handle_key(KeyEvent::Backspace), TextPromptAction::None);
    }

    #[test]
    fn enter_submits_input() {
        let mut prompt = TextPrompt::masked("password:");
        prompt.handle_key(KeyEvent::Char('s'));
        prompt.handle_key(KeyEvent::Char('e'));
        prompt.handle_key(KeyEvent::Char('c'));
        assert_eq!(
            prompt.handle_key(KeyEvent::Enter),
            TextPromptAction::Submitted("sec".into())
        );
    }

    #[test]
    fn plain_enter_submits_typed_text() {
        let mut prompt = TextPrompt::plain("address:");
        for ch in "me@gmail.com".chars() {
            prompt.handle_key(KeyEvent::Char(ch));
        }
        assert_eq!(
            prompt.handle_key(KeyEvent::Enter),
            TextPromptAction::Submitted("me@gmail.com".into())
        );
    }

    #[test]
    fn enter_on_empty_submits_empty_string() {
        let mut prompt = TextPrompt::masked("password:");
        assert_eq!(
            prompt.handle_key(KeyEvent::Enter),
            TextPromptAction::Submitted(String::new())
        );
    }
}
