//! CLI programs that can be invoked from the shell.
//!
//! Each program is a function `fn(&[&str], &mut ExecContext) -> ProgramResult`.
//! Programs live in their own submodules and are registered in [`PROGRAMS`].

use crate::http::HttpClient;
use crate::saved_networks::NetworkStore;
use crate::wifi::WifiDriver;

pub mod curl;
pub mod echo;
pub mod wifi;

/// Ambient state available to every program (clock, drivers, etc.).
pub struct ExecContext<'a> {
    pub uptime_secs: u64,
    pub wifi: &'a mut dyn WifiDriver,
    pub http: &'a mut dyn HttpClient,
    pub saved_networks: &'a mut dyn NetworkStore,
}

/// The result of running a program.
pub struct ProgramResult {
    pub output: String,
    pub exit_code: i32,
}

impl ProgramResult {
    pub fn ok(output: String) -> Self {
        Self { output, exit_code: 0 }
    }

    pub fn err(output: String) -> Self {
        Self { output, exit_code: 1 }
    }
}

/// Signature shared by all program entry-points.
pub type ProgramFn = fn(args: &[&str], ctx: &mut ExecContext) -> ProgramResult;

/// Called when the user selects an item from an interactive list.
/// `context` carries state from the command that started the list (e.g. "connect" vs "forget").
pub type OnListSelectFn = fn(context: &str, selected: &str, ctx: &mut ExecContext) -> ProgramResult;

/// Called when the user submits text in an interactive prompt.
/// `context` carries state from the prior step (e.g. selected SSID).
pub type OnTextSubmitFn = fn(context: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult;

/// A program registered in the shell.
pub struct Program {
    pub name: &'static str,
    pub usage: &'static str,
    pub run: ProgramFn,
    /// If this program can trigger a list selection, this handles the result.
    pub on_list_select: Option<OnListSelectFn>,
    /// If this program can trigger a text prompt, this handles the submitted text.
    pub on_text_submit: Option<OnTextSubmitFn>,
}

/// All available programs. The shell searches this list by name.
pub static PROGRAMS: &[Program] = &[
    Program {
        name: "curl",
        usage: "curl <url> — fetch a URL via HTTP GET",
        run: curl::run,
        on_list_select: None,
        on_text_submit: None,
    },
    Program {
        name: "echo",
        usage: "echo [args...] — print arguments",
        run: echo::run,
        on_list_select: None,
        on_text_submit: None,
    },
    Program {
        name: "wifi",
        usage: "wifi [status|connect|disconnect]",
        run: wifi::run,
        on_list_select: Some(wifi::on_list_select),
        on_text_submit: Some(wifi::on_text_submit),
    },
];
