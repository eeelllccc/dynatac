//! `echo` — print arguments separated by spaces.

use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], _ctx: &ExecContext) -> ProgramResult {
    ProgramResult::ok(args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ExecContext {
        ExecContext { uptime_secs: 0 }
    }

    #[test]
    fn no_args_prints_empty() {
        let r = run(&[], &ctx());
        assert_eq!(r.output, "");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn single_arg() {
        let r = run(&["hello"], &ctx());
        assert_eq!(r.output, "hello");
    }

    #[test]
    fn multiple_args_joined_with_spaces() {
        let r = run(&["hello", "world"], &ctx());
        assert_eq!(r.output, "hello world");
    }
}
