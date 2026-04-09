//! CLI programs that can be invoked from the shell.
//!
//! Each program is a pure function `fn(&[&str], &ExecContext) -> ProgramResult`.
//! Programs live in their own submodules and are registered in [`PROGRAMS`].

pub mod echo;

/// Ambient state available to every program (clock, system info, etc.).
pub struct ExecContext {
    pub uptime_secs: u64,
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
pub type ProgramFn = fn(args: &[&str], ctx: &ExecContext) -> ProgramResult;

/// A program registered in the shell.
pub struct Program {
    pub name: &'static str,
    pub usage: &'static str,
    pub run: ProgramFn,
}

/// All available programs. The shell searches this list by name.
pub static PROGRAMS: &[Program] = &[
    Program { name: "echo", usage: "echo [args...] — print arguments", run: echo::run },
];
