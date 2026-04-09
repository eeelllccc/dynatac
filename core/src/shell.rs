//! Shell: parses command lines and dispatches to programs.
//!
//! The shell owns two built-in commands (`help` and `clear`) and delegates
//! everything else to the program registry in [`crate::programs::PROGRAMS`].

use crate::programs::{ExecContext, PROGRAMS};

/// What the caller should do after a command executes.
pub enum ShellAction {
    /// Display this text in the terminal.
    Output(String),
    /// Clear the terminal screen.
    Clear,
}

pub struct Shell;

impl Shell {
    pub fn new() -> Self {
        Self
    }

    /// Parse `line` and execute the command. Returns an action for the caller.
    pub fn execute(&self, line: &str, ctx: &ExecContext) -> ShellAction {
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

    fn dispatch(&self, cmd: &str, args: &[&str], ctx: &ExecContext) -> ShellAction {
        for program in PROGRAMS {
            if program.name == cmd {
                let result = (program.run)(args, ctx);
                return ShellAction::Output(result.output);
            }
        }
        ShellAction::Output(format!("unknown command: {}", cmd))
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

    fn ctx() -> ExecContext {
        ExecContext { uptime_secs: 42 }
    }

    fn output(action: ShellAction) -> String {
        match action {
            ShellAction::Output(s) => s,
            ShellAction::Clear => panic!("expected Output, got Clear"),
        }
    }

    #[test]
    fn empty_input_returns_empty_output() {
        let s = Shell::new();
        assert_eq!(output(s.execute("", &ctx())), "");
        assert_eq!(output(s.execute("   ", &ctx())), "");
    }

    #[test]
    fn echo_dispatches() {
        let s = Shell::new();
        assert_eq!(output(s.execute("echo hello world", &ctx())), "hello world");
    }

    #[test]
    fn unknown_command() {
        let s = Shell::new();
        assert_eq!(output(s.execute("nosuch", &ctx())), "unknown command: nosuch");
    }

    #[test]
    fn clear_returns_clear_action() {
        let s = Shell::new();
        assert!(matches!(s.execute("clear", &ctx()), ShellAction::Clear));
    }

    #[test]
    fn help_lists_programs() {
        let s = Shell::new();
        let text = output(s.execute("help", &ctx()));
        assert!(text.contains("echo"));
        assert!(text.contains("clear"));
    }
}
