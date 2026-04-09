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
        let line = line.trim();
        if line.is_empty() {
            return ShellAction::Output(String::new());
        }

        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap();
        let args: Vec<&str> = parts.collect();

        match cmd {
            "clear" => ShellAction::Clear,
            "help" => ShellAction::Output(self.help_text()),
            _ => self.dispatch(cmd, &args, ctx),
        }
    }

    /// Handle a key event while in interactive mode.
    /// Caller should only call this when `is_interactive()` is true.
    pub fn handle_interactive_key(&mut self, key: KeyEvent, ctx: &mut ExecContext) -> ShellAction {
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

    /// Call the program's on_text_submit handler.
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

    fn start_text_prompt(&mut self, output: &str, program_name: &'static str) -> ShellAction {
        let mut lines = output.lines().skip(1); // skip __START_TEXT_PROMPT__
        let context = lines.next().unwrap_or("").to_string();
        let header = lines.next().unwrap_or("").to_string();
        let prompt = TextPrompt::new(&header);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttpClient;
    use crate::saved_networks::MockNetworkStore;
    use crate::wifi::MockWifiDriver;

    fn make_ctx<'a>(
        wifi: &'a mut dyn crate::wifi::WifiDriver,
        http: &'a mut dyn crate::http::HttpClient,
        saved: &'a mut dyn crate::saved_networks::NetworkStore,
    ) -> ExecContext<'a> {
        ExecContext { uptime_secs: 42, wifi, http, saved_networks: saved }
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
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        assert_eq!(output(s.execute("", &mut ctx)), "");
        assert_eq!(output(s.execute("   ", &mut ctx)), "");
    }

    #[test]
    fn echo_dispatches() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        assert_eq!(output(s.execute("echo hello world", &mut ctx)), "hello world");
    }

    #[test]
    fn unknown_command() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        assert_eq!(output(s.execute("nosuch", &mut ctx)), "unknown command: nosuch");
    }

    #[test]
    fn clear_returns_clear_action() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        assert!(matches!(s.execute("clear", &mut ctx), ShellAction::Clear));
    }

    #[test]
    fn help_lists_programs() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let text = output(s.execute("help", &mut ctx));
        assert!(text.contains("echo"));
        assert!(text.contains("clear"));
        assert!(text.contains("wifi"));
    }

    // --- Interactive list mode tests ---

    #[test]
    fn wifi_connect_starts_list_mode() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let text = output(s.execute("wifi connect", &mut ctx));
        assert!(s.is_interactive());
        assert!(text.contains("select network:"));
        assert!(text.contains("> home_wifi"));
    }

    #[test]
    fn list_mode_h_moves_cursor_down() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        s.execute("wifi connect", &mut ctx);

        let text = output(s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx));
        assert!(text.contains("> coffee_shop"));
        assert!(text.contains("  home_wifi"));
    }

    #[test]
    fn list_mode_y_moves_cursor_up() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        s.execute("wifi connect", &mut ctx);

        s.handle_interactive_key(KeyEvent::Char('h'), &mut ctx);
        let text = output(s.handle_interactive_key(KeyEvent::Char('y'), &mut ctx));
        assert!(text.contains("> home_wifi"));
    }

    #[test]
    fn list_mode_enter_transitions_to_text_prompt() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
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
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
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
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        s.execute("wifi connect", &mut ctx);

        let text = output(s.handle_interactive_key(KeyEvent::Char('x'), &mut ctx));
        assert_eq!(text, "");
        assert!(s.is_interactive());
    }

    // --- Integration: full wifi flow ---

    #[test]
    fn full_wifi_flow() {
        let mut s = Shell::new();
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);

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
