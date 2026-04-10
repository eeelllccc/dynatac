//! Shell: parses command lines and dispatches to programs.
//!
//! The shell owns two built-in commands (`help` and `clear`) and delegates
//! everything else to the program registry in [`crate::programs::PROGRAMS`].
//!
//! The shell can enter interactive list mode when a program returns a
//! `__START_LIST__` output. In this mode, keys are routed to a
//! [`ListSelector`] instead of the terminal input. When the user selects
//! an item, the shell calls the program's `on_list_select` handler.

use crate::keymap::KeyEvent;
use crate::list_selector::{ListAction, ListSelector};
use crate::programs::{ExecContext, PROGRAMS};
use crate::text_prompt::{TextPrompt, TextPromptAction};

/// What the caller should do after a command executes.
#[derive(Debug)]
pub enum ShellAction {
    /// Display this text in the terminal.
    Output(String),
    /// Clear the terminal screen.
    Clear,
}

/// Which interactive mode the shell is in.
enum InteractiveState {
    /// Navigating a list of options. Carries the program name and a context
    /// string from the command that started the list (e.g. "connect" vs "forget").
    List(ListSelector, &'static str, String),
    /// Typing into a text prompt. Carries the program name and context
    /// from the previous step (e.g. the selected SSID).
    TextPrompt(TextPrompt, &'static str, String),
}

pub struct Shell {
    interactive: Option<InteractiveState>,
}

impl Shell {
    pub fn new() -> Self {
        Self { interactive: None }
    }

    /// Whether the shell is in an interactive mode (list or text prompt).
    pub fn is_interactive(&self) -> bool {
        self.interactive.is_some()
    }

    /// Parse `line` and execute the command. Returns an action for the caller.
    pub fn execute(&mut self, line: &str, ctx: &mut ExecContext) -> ShellAction {
        let tokens = match tokenize(line) {
            Ok(t) => t,
            Err(e) => return ShellAction::Output(format!("parse error: {}", e)),
        };
        if tokens.is_empty() {
            return ShellAction::Output(String::new());
        }

        let cmd = tokens[0].as_str();
        let args: Vec<&str> = tokens[1..].iter().map(|s| s.as_str()).collect();

        match cmd {
            "clear" => ShellAction::Clear,
            "help" => ShellAction::Output(self.help_text()),
            _ => self.dispatch(cmd, &args, ctx),
        }
    }

    /// Handle a key event while in interactive mode.
    /// Caller should only call this when `is_interactive()` is true.
    pub fn handle_interactive_key(&mut self, key: KeyEvent, ctx: &mut ExecContext) -> ShellAction {
        // Alt+Backspace cancels any active interactive mode. We intercept
        // it here so the inner widgets (list selector, text prompt) and
        // their host programs don't need to know about cancellation.
        if matches!(key, KeyEvent::Cancel) {
            self.interactive = None;
            return ShellAction::Output("cancelled".to_string());
        }
        match &mut self.interactive {
            Some(InteractiveState::List(selector, program_name, context)) => {
                let program_name = *program_name;
                let context = context.clone();
                match selector.handle_key(key) {
                    ListAction::Redraw => ShellAction::Output(selector.render()),
                    ListAction::Selected(item) => {
                        self.interactive = None;
                        self.handle_list_selection(&context, &item, program_name, ctx)
                    }
                    ListAction::None => ShellAction::Output(String::new()),
                }
            }
            Some(InteractiveState::TextPrompt(prompt, program_name, context)) => {
                let program_name = *program_name;
                let context = context.clone();
                match prompt.handle_key(key) {
                    TextPromptAction::Redraw => ShellAction::Output(prompt.render()),
                    TextPromptAction::Submitted(text) => {
                        self.interactive = None;
                        self.handle_text_submit(&context, &text, program_name, ctx)
                    }
                    TextPromptAction::None => ShellAction::Output(String::new()),
                }
            }
            None => ShellAction::Output(String::new()),
        }
    }

    /// Process the result of a list selection. If the program's on_list_select
    /// returns a text prompt signal, transition to text prompt mode.
    fn handle_list_selection(
        &mut self,
        context: &str,
        item: &str,
        program_name: &'static str,
        ctx: &mut ExecContext,
    ) -> ShellAction {
        for program in PROGRAMS {
            if program.name == program_name {
                if let Some(handler) = program.on_list_select {
                    let result = handler(context, item, ctx);
                    if result.output.starts_with("__START_TEXT_PROMPT__\n") {
                        return self.start_text_prompt(&result.output, program_name);
                    }
                    return ShellAction::Output(result.output);
                }
            }
        }
        ShellAction::Output(format!("selected: {}", item))
    }

    /// Call the program's on_text_submit handler. The handler may itself
    /// signal a transition into another text prompt or list (e.g.
    /// `email setup` chains address → password prompts), so check the
    /// output for both signals.
    fn handle_text_submit(
        &mut self,
        context: &str,
        text: &str,
        program_name: &'static str,
        ctx: &mut ExecContext,
    ) -> ShellAction {
        for program in PROGRAMS {
            if program.name == program_name {
                if let Some(handler) = program.on_text_submit {
                    let result = handler(context, text, ctx);
                    if result.output.starts_with("__START_TEXT_PROMPT__\n") {
                        return self.start_text_prompt(&result.output, program_name);
                    }
                    if result.output.starts_with("__START_LIST__\n") {
                        return self.start_list(&result.output, program_name);
                    }
                    return ShellAction::Output(result.output);
                }
            }
        }
        ShellAction::Output(String::new())
    }

    fn dispatch(&mut self, cmd: &str, args: &[&str], ctx: &mut ExecContext) -> ShellAction {
        for program in PROGRAMS {
            if program.name == cmd {
                let result = (program.run)(args, ctx);
                if result.output.starts_with("__START_LIST__\n") {
                    return self.start_list(&result.output, program.name);
                }
                if result.output.starts_with("__START_TEXT_PROMPT__\n") {
                    return self.start_text_prompt(&result.output, program.name);
                }
                return ShellAction::Output(result.output);
            }
        }
        ShellAction::Output(format!("unknown command: {}", cmd))
    }

    fn start_list(&mut self, output: &str, program_name: &'static str) -> ShellAction {
        let mut lines = output.lines().skip(1); // skip __START_LIST__
        let context = lines.next().unwrap_or("").to_string();
        let header = lines.next().unwrap_or("").to_string();
        let items: Vec<String> = lines.map(|l| l.to_string()).collect();
        if items.is_empty() {
            return ShellAction::Output("no items to select".to_string());
        }
        let selector = ListSelector::new(&header, items);
        let rendered = selector.render();
        self.interactive = Some(InteractiveState::List(selector, program_name, context));
        ShellAction::Output(rendered)
    }

    /// Signal format:
    /// ```text
    /// __START_TEXT_PROMPT__
    /// <mode>            // "mask" or "plain"
    /// <context>         // opaque string passed back to on_text_submit
    /// <header>          // displayed above the input line
    /// ```
    fn start_text_prompt(&mut self, output: &str, program_name: &'static str) -> ShellAction {
        let mut lines = output.lines().skip(1); // skip __START_TEXT_PROMPT__
        let mode = lines.next().unwrap_or("mask");
        let context = lines.next().unwrap_or("").to_string();
        let header = lines.next().unwrap_or("").to_string();
        let prompt = match mode {
            "plain" => TextPrompt::plain(&header),
            // Default to masked: safer if a caller forgets the mode line.
            _ => TextPrompt::masked(&header),
        };
        let rendered = prompt.render();
        self.interactive = Some(InteractiveState::TextPrompt(
            prompt,
            program_name,
            context,
        ));
        ShellAction::Output(rendered)
    }

    fn help_text(&self) -> String {
        let mut lines: Vec<String> = Vec::with_capacity(PROGRAMS.len() + 2);
        lines.push("built-in: help, clear".to_string());
        for p in PROGRAMS {
            lines.push(p.usage.to_string());
        }
        lines.join("\n")
    }
}

/// Split a command line into tokens.
///
/// Rules:
///   - Whitespace (including `\n`) separates tokens outside of quotes.
///   - Double-quoted regions preserve all characters verbatim, including
///     spaces and newlines, until the matching closing quote.
///   - Inside quotes, `\"` is a literal quote and `\\` is a literal backslash.
///   - A bare token may be adjacent to a quoted region (`foo"bar"baz`
///     becomes the single token `foobarbaz`).
///   - An unterminated quoted string is an error.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escape = false;
    // Tracks whether `current` is part of an active token. Needed to
    // distinguish "no token yet" from "empty quoted token".
    let mut has_token = false;

    for c in line.chars() {
        if escape {
            current.push(c);
            escape = false;
            continue;
        }
        if in_quotes {
            match c {
                '\\' => escape = true,
                '"' => in_quotes = false,
                _ => current.push(c),
            }
        } else if c == '"' {
            in_quotes = true;
            has_token = true;
        } else if c.is_whitespace() {
            if has_token {
                tokens.push(std::mem::take(&mut current));
                has_token = false;
            }
        } else {
            current.push(c);
            has_token = true;
        }
    }

    if in_quotes {
        return Err("unterminated string".to_string());
    }
    if has_token {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{CredentialStore, MockCredentialStore};
    use crate::email::MockSmtpStreamFactory;
    use crate::http::MockHttpClient;
    use crate::saved_networks::{MockNetworkStore, NetworkStore};
    use crate::wifi::MockWifiDriver;

    struct Env {
        wifi: MockWifiDriver,
        http: MockHttpClient,
        saved: MockNetworkStore,
        smtp: MockSmtpStreamFactory,
        creds: MockCredentialStore,
    }

    impl Env {
        fn new() -> Self {
            Self {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
            }
        }
        fn ctx(&mut self) -> ExecContext<'_> {
            ExecContext {
                uptime_secs: 42,
                wifi: &mut self.wifi,
                http: &mut self.http,
                saved_networks: &mut self.saved,
                smtp: &mut self.smtp,
                credentials: &mut self.creds,
            }
        }
    }

    fn output(action: ShellAction) -> String {
        match action {
            ShellAction::Output(s) => s,
            ShellAction::Clear => panic!("expected Output, got Clear"),
        }
    }

    #[test]
    fn empty_input_returns_empty_output() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        assert_eq!(output(s.execute("", &mut ctx)), "");
        assert_eq!(output(s.execute("   ", &mut ctx)), "");
    }

    #[test]
    fn echo_dispatches() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        assert_eq!(output(s.execute("echo hello world", &mut ctx)), "hello world");
    }

    #[test]
    fn unknown_command() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        assert_eq!(output(s.execute("nosuch", &mut ctx)), "unknown command: nosuch");
    }

    #[test]
    fn clear_returns_clear_action() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        assert!(matches!(s.execute("clear", &mut ctx), ShellAction::Clear));
    }

    #[test]
    fn help_lists_programs() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        let text = output(s.execute("help", &mut ctx));
        assert!(text.contains("echo"));
        assert!(text.contains("clear"));
        assert!(text.contains("wifi"));
    }

    // --- Interactive list mode tests ---

    #[test]
    fn wifi_connect_starts_list_mode() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        let text = output(s.execute("wifi connect", &mut ctx));
        assert!(s.is_interactive());
        assert!(text.contains("select network:"));
        assert!(text.contains("> home_wifi"));
    }

    #[test]
    fn list_mode_h_moves_cursor_down() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);

        let text = output(s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx));
        assert!(text.contains("> coffee_shop"));
        assert!(text.contains("  home_wifi"));
    }

    #[test]
    fn list_mode_y_moves_cursor_up() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);

        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
        let text = output(s.handle_interactive_key(KeyEvent::Char('y'), &mut ctx));
        assert!(text.contains("> home_wifi"));
    }

    #[test]
    fn list_mode_enter_transitions_to_text_prompt() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);

        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx); // move to coffee_shop
        let text = output(s.handle_interactive_key(KeyEvent::Enter, &mut ctx));
        // Should now be in text prompt mode, not connected yet
        assert!(s.is_interactive());
        assert!(text.contains("password:"));
    }

    #[test]
    fn text_prompt_submit_connects() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);

        // Select coffee_shop
        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Enter, &mut ctx);
        assert!(s.is_interactive()); // in text prompt mode

        // Type password and submit
        s.handle_interactive_key(KeyEvent::Char('p'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Char('w'), &mut ctx);
        let text = output(s.handle_interactive_key(KeyEvent::Enter, &mut ctx));
        assert_eq!(text, "connected to coffee_shop");
        assert!(!s.is_interactive());
    }

    #[test]
    fn list_mode_irrelevant_key_returns_empty() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);

        let text = output(s.handle_interactive_key(KeyEvent::Char('x'), &mut ctx));
        assert_eq!(text, "");
        assert!(s.is_interactive());
    }

    // --- Direct text-prompt entry from a program's run() ---

    #[test]
    fn email_setup_enters_text_prompt_mode() {
        // Regression: previously the shell only intercepted __START_LIST__
        // from a program's run(), so `email setup` (which returns
        // __START_TEXT_PROMPT__ directly) leaked the raw signal to the
        // terminal and the next typed line was parsed as an unknown command.
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        let text = output(s.execute("email setup", &mut ctx));
        assert!(s.is_interactive(), "shell should be in interactive mode");
        // The signal must NOT appear in the rendered output.
        assert!(
            !text.contains("__START_TEXT_PROMPT__"),
            "raw signal leaked: {}",
            text
        );
        assert!(text.contains("Gmail address:"));
    }

    #[test]
    fn email_setup_full_flow_via_shell() {
        // End-to-end through the shell: setup → address prompt → password
        // prompt → save.
        let mut s = Shell::new();
        let mut env = Env::new();
        {
            let mut ctx = env.ctx();
            output(s.execute("email setup", &mut ctx));
            assert!(s.is_interactive());

            // Type "x@y" then Enter
            for ch in "x@y".chars() {
                s.handle_interactive_key(KeyEvent::Char(ch), &mut ctx);
            }
            let text = output(s.handle_interactive_key(KeyEvent::Enter, &mut ctx));
            assert!(s.is_interactive(), "should still be interactive (password prompt)");
            assert!(text.contains("App password:"));

            // Type "pw" then Enter
            for ch in "pw".chars() {
                s.handle_interactive_key(KeyEvent::Char(ch), &mut ctx);
            }
            let text = output(s.handle_interactive_key(KeyEvent::Enter, &mut ctx));
            assert!(!s.is_interactive());
            assert!(text.contains("saved gmail account: x@y"));
        }
        let creds = env.creds.gmail().unwrap();
        assert_eq!(creds.address, "x@y");
        assert_eq!(creds.app_password, "pw");
    }

    // --- Cancel tests ---

    #[test]
    fn cancel_in_list_mode_exits_interactive() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);
        assert!(s.is_interactive());

        let text = output(s.handle_interactive_key(KeyEvent::Cancel, &mut ctx));
        assert_eq!(text, "cancelled");
        assert!(!s.is_interactive());
    }

    #[test]
    fn cancel_in_text_prompt_mode_exits_interactive() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        s.execute("wifi connect", &mut ctx);
        // Pick coffee_shop (no saved password) → text prompt for password
        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Enter, &mut ctx);
        assert!(s.is_interactive());

        // Type a few chars then cancel
        s.handle_interactive_key(KeyEvent::Char('a'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Char('b'), &mut ctx);
        let text = output(s.handle_interactive_key(KeyEvent::Cancel, &mut ctx));
        assert_eq!(text, "cancelled");
        assert!(!s.is_interactive());
    }

    #[test]
    fn cancel_does_not_save_partial_state() {
        // After cancelling a wifi connect mid-flow, the network must not be
        // connected and the password must not be saved.
        let mut s = Shell::new();
        let mut env = Env::new();
        {
            let mut ctx = env.ctx();
            s.execute("wifi connect", &mut ctx);
            s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
            s.handle_interactive_key(KeyEvent::Enter, &mut ctx);
            s.handle_interactive_key(KeyEvent::Char('p'), &mut ctx);
            s.handle_interactive_key(KeyEvent::Cancel, &mut ctx);
        }
        assert!(!s.is_interactive());
        assert_eq!(env.saved.load("coffee_shop"), None);
    }

    // --- Tokenizer tests ---

    #[test]
    fn tok_bare_args_split_on_whitespace() {
        assert_eq!(tokenize("echo a b c").unwrap(), vec!["echo", "a", "b", "c"]);
    }

    #[test]
    fn tok_empty_input_returns_empty_vec() {
        assert!(tokenize("").unwrap().is_empty());
        assert!(tokenize("   ").unwrap().is_empty());
        assert!(tokenize("\n\n").unwrap().is_empty());
    }

    #[test]
    fn tok_quoted_arg_with_spaces() {
        assert_eq!(
            tokenize("echo \"hello world\"").unwrap(),
            vec!["echo", "hello world"]
        );
    }

    #[test]
    fn tok_quoted_arg_preserves_newlines() {
        let input = "email \"line1\nline2\"";
        assert_eq!(
            tokenize(input).unwrap(),
            vec!["email", "line1\nline2"]
        );
    }

    #[test]
    fn tok_newline_outside_quotes_is_whitespace() {
        let input = "email\nfoo@bar.com\n\"sub\"\n\"body\"";
        assert_eq!(
            tokenize(input).unwrap(),
            vec!["email", "foo@bar.com", "sub", "body"]
        );
    }

    #[test]
    fn tok_escaped_quote_inside_quoted_arg() {
        assert_eq!(
            tokenize("echo \"he said \\\"hi\\\"\"").unwrap(),
            vec!["echo", "he said \"hi\""]
        );
    }

    #[test]
    fn tok_escaped_backslash_inside_quoted_arg() {
        assert_eq!(
            tokenize("echo \"a\\\\b\"").unwrap(),
            vec!["echo", "a\\b"]
        );
    }

    #[test]
    fn tok_unterminated_quote_returns_error() {
        let err = tokenize("echo \"oops").unwrap_err();
        assert!(err.contains("unterminated"));
    }

    #[test]
    fn tok_mixed_quoted_and_bare() {
        assert_eq!(
            tokenize("email foo@bar.com \"subject\" \"body\"").unwrap(),
            vec!["email", "foo@bar.com", "subject", "body"]
        );
    }

    #[test]
    fn tok_full_multiline_email_example() {
        // The example from the spec — typed across several lines via Shift+Enter.
        let input = "email\nsome@addre.com\n\"subject here\"\n\"Multiline body\ncan go here\n\nthanks,\nfrom me\"";
        assert_eq!(
            tokenize(input).unwrap(),
            vec![
                "email",
                "some@addre.com",
                "subject here",
                "Multiline body\ncan go here\n\nthanks,\nfrom me",
            ]
        );
    }

    #[test]
    fn tok_adjacent_bare_and_quoted_concatenate() {
        assert_eq!(
            tokenize("foo\"bar\"baz").unwrap(),
            vec!["foobarbaz"]
        );
    }

    // --- Shell integration: parse error path ---

    #[test]
    fn parse_error_returns_error_output() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        let text = output(s.execute("echo \"oops", &mut ctx));
        assert!(text.contains("parse error"));
        assert!(text.contains("unterminated"));
    }

    #[test]
    fn quoted_arg_passes_through_to_program() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();
        // echo joins its args with " " — quoted "hello world" is one arg.
        assert_eq!(
            output(s.execute("echo \"hello world\"", &mut ctx)),
            "hello world"
        );
    }

    // --- Integration: full wifi flow ---

    #[test]
    fn full_wifi_flow() {
        let mut s = Shell::new();
        let mut env = Env::new();
        let mut ctx = env.ctx();

        // Initially not connected
        let text = output(s.execute("wifi status", &mut ctx));
        assert_eq!(text, "not connected");

        // Start connect flow
        s.execute("wifi connect", &mut ctx);
        assert!(s.is_interactive());

        // Navigate to neighbor_5g (index 2) and select
        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Enter, &mut ctx);

        // Now in text prompt mode — type password and submit
        assert!(s.is_interactive());
        s.handle_interactive_key(KeyEvent::Char('s'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Char('e'), &mut ctx);
        s.handle_interactive_key(KeyEvent::Char('c'), &mut ctx);
        let text = output(s.handle_interactive_key(KeyEvent::Enter, &mut ctx));
        assert_eq!(text, "connected to neighbor_5g");
        assert!(!s.is_interactive());

        // Verify connected
        let text = output(s.execute("wifi status", &mut ctx));
        assert_eq!(text, "connected to neighbor_5g");

        // Disconnect
        let text = output(s.execute("wifi disconnect", &mut ctx));
        assert_eq!(text, "disconnected");

        // Verify disconnected
        let text = output(s.execute("wifi status", &mut ctx));
        assert_eq!(text, "not connected");
    }
}
