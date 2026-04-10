//! `email` — send a plain-text email via Gmail SMTP submission, plus
//! the `email setup` subcommand for configuring the Gmail account.
//!
//! Usage:
//!   email <to> "<subject>" "<body>"   send a message
//!   email setup                       configure / replace the gmail account
//!
//! ## Send flow
//! Reads the configured Gmail account from `ctx.credentials`. If none is
//! set, prints a hint to run `email setup` first. Opens an SMTP stream
//! via `ctx.smtp` (production: TLS to smtp.gmail.com:465; tests: a
//! scripted mock) and runs an [`SmtpSession`] to deliver the message.
//!
//! ## Setup flow
//! `email setup` is a two-step interactive text-prompt flow:
//!   1. `run(["setup"])` returns a text-prompt signal with context
//!      `"address"`. The header advertises the address being replaced if
//!      one is already configured.
//!   2. `on_text_submit("address", text)` records the new address inside
//!      the *next* prompt's context (`"password|<address>"`) and returns
//!      another text-prompt signal.
//!   3. `on_text_submit("password|<address>", text)` writes both fields
//!      to the credential store.
//!
//! At either step, submitting an empty string acts as a soft cancel:
//!   - If credentials are already configured, the existing values are
//!     kept and the user sees `cancelled — kept <address>`.
//!   - If no credentials are configured, an empty value is rejected.
//!
//! Pressing Alt+Backspace at any point exits interactive mode entirely
//! (handled by the shell, not this program).

use super::{ExecContext, ProgramResult};
use crate::email::{Email, SmtpSession};

const GMAIL_HOST: &str = "smtp.gmail.com";
const GMAIL_PORT: u16 = 465;
/// Hostname we announce in EHLO. Doesn't need to resolve; Gmail accepts
/// arbitrary identifiers from authenticated submission clients.
const EHLO_NAME: &str = "dynatac.local";

const ADDRESS_CONTEXT: &str = "address";
const PASSWORD_CONTEXT_PREFIX: &str = "password|";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args {
        ["setup"] => start_setup(ctx),
        [to, subject, body] => send(to, subject, body, ctx),
        _ => ProgramResult::err(usage()),
    }
}

fn usage() -> String {
    "usage: email <to> \"<subject>\" \"<body>\"\n       email setup".to_string()
}

// --- Send -------------------------------------------------------------------

fn send(to: &str, subject: &str, body: &str, ctx: &mut ExecContext) -> ProgramResult {
    let creds = match ctx.credentials.gmail() {
        Some(c) => c,
        None => {
            return ProgramResult::err(
                "no gmail account configured — run: email setup".to_string(),
            );
        }
    };
    let email = Email {
        from: creds.address.clone(),
        to: to.to_string(),
        subject: subject.to_string(),
        body: body.to_string(),
    };

    let stream = match ctx.smtp.open(GMAIL_HOST, GMAIL_PORT) {
        Ok(s) => s,
        Err(e) => return ProgramResult::err(format!("connect: {}", e)),
    };
    match SmtpSession::send(
        stream,
        EHLO_NAME,
        &creds.address,
        &creds.app_password,
        &email,
    ) {
        Ok(()) => ProgramResult::ok(format!("sent to {}", to)),
        Err(e) => ProgramResult::err(format!("send failed: {}", e)),
    }
}

// --- Setup ------------------------------------------------------------------

fn start_setup(ctx: &mut ExecContext) -> ProgramResult {
    let header = match ctx.credentials.gmail() {
        Some(creds) => format!("Gmail address (replacing {}):", creds.address),
        None => "Gmail address:".to_string(),
    };
    ProgramResult::ok(format!(
        "__START_TEXT_PROMPT__\n{}\n{}",
        ADDRESS_CONTEXT, header
    ))
}

pub fn on_text_submit(context: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult {
    if context == ADDRESS_CONTEXT {
        return on_address_submit(text, ctx);
    }
    if let Some(address) = context.strip_prefix(PASSWORD_CONTEXT_PREFIX) {
        return on_password_submit(address, text, ctx);
    }
    ProgramResult::err(format!("email: unexpected prompt context {:?}", context))
}

fn on_address_submit(text: &str, ctx: &mut ExecContext) -> ProgramResult {
    let address = text.trim();
    if address.is_empty() {
        return cancel_or_empty_error(ctx);
    }
    let header = if ctx.credentials.gmail().is_some() {
        "App password (empty to cancel):".to_string()
    } else {
        "App password:".to_string()
    };
    ProgramResult::ok(format!(
        "__START_TEXT_PROMPT__\n{}{}\n{}",
        PASSWORD_CONTEXT_PREFIX, address, header
    ))
}

fn on_password_submit(address: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult {
    let password = text.trim();
    if password.is_empty() {
        return cancel_or_empty_error(ctx);
    }
    match ctx.credentials.set_gmail(address, password) {
        Ok(()) => ProgramResult::ok(format!("saved gmail account: {}", address)),
        Err(e) => ProgramResult::err(format!("save failed: {}", e)),
    }
}

/// Empty-input handler shared by both setup steps. If credentials already
/// exist, this is a soft cancel that leaves them in place. Otherwise it's
/// a hard error — the user has to type something to make progress.
fn cancel_or_empty_error(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.credentials.gmail() {
        Some(creds) => ProgramResult::ok(format!("cancelled — kept {}", creds.address)),
        None => ProgramResult::err("cannot be empty".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::{CredentialStore, MockCredentialStore};
    use crate::email::MockSmtpStreamFactory;
    use crate::http::MockHttpClient;
    use crate::saved_networks::MockNetworkStore;
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
                uptime_secs: 0,
                wifi: &mut self.wifi,
                http: &mut self.http,
                saved_networks: &mut self.saved,
                smtp: &mut self.smtp,
                credentials: &mut self.creds,
            }
        }
    }

    fn install_happy_server(env: &mut Env) {
        env.smtp
            .stream
            .push_line("220 smtp.gmail.com ready")
            .push_line("250-smtp.gmail.com Hello")
            .push_line("250-AUTH LOGIN PLAIN")
            .push_line("250 OK")
            .push_line("334 VXNlcm5hbWU6")
            .push_line("334 UGFzc3dvcmQ6")
            .push_line("235 2.7.0 Accepted")
            .push_line("250 2.1.0 OK")
            .push_line("250 2.1.5 OK")
            .push_line("354 Go ahead")
            .push_line("250 2.0.0 OK queued")
            .push_line("221 2.0.0 closing");
    }

    // --- Send: argument validation -------------------------------------------

    #[test]
    fn no_args_shows_usage() {
        let mut env = Env::new();
        let r = run(&[], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("usage"));
        // Both modes mentioned
        assert!(r.output.contains("setup"));
    }

    #[test]
    fn wrong_arg_count_shows_usage() {
        let mut env = Env::new();
        for args in [
            &["only-one"][..],
            &["a", "b"][..],
            &["a", "b", "c", "d"][..],
        ] {
            let r = run(args, &mut env.ctx());
            assert_eq!(r.exit_code, 1, "args = {:?}", args);
            assert!(r.output.contains("usage"));
        }
    }

    #[test]
    fn missing_credentials_hints_at_setup() {
        let mut env = Env::new();
        let r = run(&["a@b.com", "sub", "body"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("email setup"));
    }

    // --- Send: happy path ----------------------------------------------------

    #[test]
    fn happy_path_sends_and_reports_success() {
        let mut env = Env::new();
        env.creds
            .set_gmail("me@gmail.com", "abcdefghijklmnop")
            .unwrap();
        install_happy_server(&mut env);

        let r = run(
            &["you@example.com", "hi", "hello there"],
            &mut env.ctx(),
        );
        assert_eq!(r.exit_code, 0, "output was: {}", r.output);
        assert!(r.output.contains("sent to you@example.com"));

        assert_eq!(env.smtp.open_count, 1);
        let written = env.smtp.stream.written_str();
        assert!(written.contains("MAIL FROM:<me@gmail.com>\r\n"));
        assert!(written.contains("RCPT TO:<you@example.com>\r\n"));
        assert!(written.contains("Subject: hi\r\n"));
        assert!(written.contains("\r\nhello there\r\n.\r\n"));
    }

    #[test]
    fn multiline_body_is_preserved_on_the_wire() {
        let mut env = Env::new();
        env.creds.set_gmail("me@gmail.com", "pw").unwrap();
        install_happy_server(&mut env);

        let body = "first line\nsecond line\n\nfourth";
        let r = run(&["you@example.com", "subj", body], &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output was: {}", r.output);

        let written = env.smtp.stream.written_str();
        assert!(written.contains("first line\r\nsecond line\r\n\r\nfourth\r\n.\r\n"));
    }

    // --- Send: failure paths -------------------------------------------------

    #[test]
    fn connect_failure_is_reported() {
        let mut env = Env::new();
        env.creds.set_gmail("me@gmail.com", "pw").unwrap();
        env.smtp.fail_with = Some("tls handshake failed".to_string());

        let r = run(&["you@example.com", "s", "b"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("connect"));
        assert!(r.output.contains("tls handshake failed"));
    }

    #[test]
    fn send_failure_is_reported() {
        let mut env = Env::new();
        env.creds.set_gmail("me@gmail.com", "pw").unwrap();
        env.smtp.stream.push_line("554 go away");

        let r = run(&["you@example.com", "s", "b"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("send failed"));
        assert!(r.output.contains("554"));
    }

    // --- Setup: first-time flow ---------------------------------------------

    #[test]
    fn setup_first_time_starts_address_prompt() {
        let mut env = Env::new();
        let r = run(&["setup"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_TEXT_PROMPT__");
        assert_eq!(lines[1], "address");
        assert_eq!(lines[2], "Gmail address:");
    }

    #[test]
    fn setup_address_submit_prompts_for_password_carrying_address_in_context() {
        let mut env = Env::new();
        let r = on_text_submit("address", "me@gmail.com", &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_TEXT_PROMPT__");
        assert_eq!(lines[1], "password|me@gmail.com");
        assert_eq!(lines[2], "App password:");
    }

    #[test]
    fn setup_password_submit_saves_credentials_and_confirms() {
        let mut env = Env::new();
        let r = on_text_submit(
            "password|me@gmail.com",
            "abcdefghijklmnop",
            &mut env.ctx(),
        );
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("saved gmail account: me@gmail.com"));
        let creds = env.creds.gmail().unwrap();
        assert_eq!(creds.address, "me@gmail.com");
        assert_eq!(creds.app_password, "abcdefghijklmnop");
    }

    #[test]
    fn setup_first_time_empty_address_rejected() {
        let mut env = Env::new();
        let r = on_text_submit("address", "   ", &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("empty"));
    }

    #[test]
    fn setup_first_time_empty_password_rejected() {
        let mut env = Env::new();
        let r = on_text_submit("password|me@gmail.com", "  ", &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("empty"));
        assert!(env.creds.gmail().is_none());
    }

    #[test]
    fn setup_unexpected_context_errors() {
        let mut env = Env::new();
        let r = on_text_submit("nonsense", "x", &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unexpected"));
    }

    // --- Setup: replacing an existing account --------------------------------

    #[test]
    fn setup_with_existing_creds_shows_replacing_in_address_header() {
        let mut env = Env::new();
        env.creds.set_gmail("old@gmail.com", "oldpw").unwrap();
        let r = run(&["setup"], &mut env.ctx());
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_TEXT_PROMPT__");
        assert_eq!(lines[1], "address");
        assert_eq!(lines[2], "Gmail address (replacing old@gmail.com):");
    }

    #[test]
    fn setup_with_existing_creds_password_header_mentions_cancel() {
        let mut env = Env::new();
        env.creds.set_gmail("old@gmail.com", "oldpw").unwrap();
        let r = on_text_submit("address", "new@gmail.com", &mut env.ctx());
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[2], "App password (empty to cancel):");
    }

    #[test]
    fn setup_empty_address_cancels_when_creds_exist() {
        let mut env = Env::new();
        env.creds.set_gmail("old@gmail.com", "oldpw").unwrap();
        let r = on_text_submit("address", "", &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("cancelled"));
        assert!(r.output.contains("old@gmail.com"));
        // Old creds untouched
        let creds = env.creds.gmail().unwrap();
        assert_eq!(creds.address, "old@gmail.com");
        assert_eq!(creds.app_password, "oldpw");
    }

    #[test]
    fn setup_empty_password_cancels_when_creds_exist() {
        let mut env = Env::new();
        env.creds.set_gmail("old@gmail.com", "oldpw").unwrap();
        // The user reached the password prompt with a new address typed,
        // then hit Enter on empty — that should cancel and keep the OLD
        // credentials, not the half-entered new ones.
        let r = on_text_submit("password|new@gmail.com", "", &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("cancelled"));
        assert!(r.output.contains("old@gmail.com"));
        let creds = env.creds.gmail().unwrap();
        assert_eq!(creds.address, "old@gmail.com");
        assert_eq!(creds.app_password, "oldpw");
    }

    #[test]
    fn setup_replacing_completes_overwrites() {
        let mut env = Env::new();
        env.creds.set_gmail("old@gmail.com", "oldpw").unwrap();

        // address step
        let r = on_text_submit("address", "new@gmail.com", &mut env.ctx());
        assert_eq!(r.exit_code, 0);

        // password step
        let r = on_text_submit("password|new@gmail.com", "newpw", &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("saved"));
        let creds = env.creds.gmail().unwrap();
        assert_eq!(creds.address, "new@gmail.com");
        assert_eq!(creds.app_password, "newpw");
    }
}
