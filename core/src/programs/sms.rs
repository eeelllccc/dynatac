//! `sms` — send and read text messages over the cellular modem.
//!
//! Subcommands:
//!   - `sms send <number> <body...>` — send a text message
//!   - `sms inbox`                   — list messages stored on the SIM
//!   - `sms read <index>`            — read a single message
//!   - `sms delete <index>`          — delete a single message
//!
//! The modem must be powered on (`modem on`) before any of these will work
//! — the cellular radio is the most power-hungry thing on the device, so
//! it's deliberately not auto-started at boot.

use crate::sms::{self, SmsMessage, SmsStatus};

use super::{ExecContext, ProgramResult};

pub const USAGE: &str = "sms [send <number> <body>|inbox|read <idx>|delete <idx>]";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("send") => send(&args[1..], ctx),
        Some("inbox") => inbox(ctx),
        Some("read") => read(&args[1..], ctx),
        Some("delete") => delete(&args[1..], ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
        None => ProgramResult::ok(USAGE.to_string()),
    }
}

fn send(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    if args.len() < 2 {
        return ProgramResult::err("usage: sms send <number> <body>".to_string());
    }
    let to = args[0];
    // Allow either `sms send +447 "hello world"` (one quoted arg) or
    // `sms send +447 hello world` (multiple bare args). Both produce
    // the same logical body.
    let body = args[1..].join(" ");
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off — run `modem on` first".to_string());
    }
    match sms::send_text(ctx.modem, to, &body) {
        Ok(()) => ProgramResult::ok(format!("sent to {}", to)),
        Err(e) => ProgramResult::err(format!("send failed: {}", e.display())),
    }
}

fn inbox(ctx: &mut ExecContext) -> ProgramResult {
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off — run `modem on` first".to_string());
    }
    match sms::list_inbox(ctx.modem) {
        Ok(msgs) if msgs.is_empty() => ProgramResult::ok("(no messages)".to_string()),
        Ok(msgs) => ProgramResult::ok(format_inbox(&msgs)),
        Err(e) => ProgramResult::err(e.display()),
    }
}

fn read(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    let index = match parse_index(args) {
        Ok(i) => i,
        Err(e) => return ProgramResult::err(e),
    };
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off — run `modem on` first".to_string());
    }
    match sms::read_message(ctx.modem, index) {
        Ok(msg) => ProgramResult::ok(format_message(&msg)),
        Err(e) => ProgramResult::err(e.display()),
    }
}

fn delete(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    let index = match parse_index(args) {
        Ok(i) => i,
        Err(e) => return ProgramResult::err(e),
    };
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off — run `modem on` first".to_string());
    }
    match sms::delete_message(ctx.modem, index) {
        Ok(()) => ProgramResult::ok(format!("deleted message {}", index)),
        Err(e) => ProgramResult::err(e.display()),
    }
}

fn parse_index(args: &[&str]) -> Result<u32, String> {
    let raw = args
        .first()
        .ok_or_else(|| "usage: requires <index>".to_string())?;
    raw.parse::<u32>()
        .map_err(|_| format!("invalid index: {}", raw))
}

fn format_inbox(msgs: &[SmsMessage]) -> String {
    let mut lines = Vec::new();
    for msg in msgs {
        lines.push(format!(
            "[{}] {} {}",
            msg.index,
            status_marker(&msg.status),
            msg.sender
        ));
        lines.push(format!("    {}", msg.body));
    }
    lines.join("\n")
}

fn format_message(msg: &SmsMessage) -> String {
    let mut lines = Vec::new();
    lines.push(format!("from: {}", msg.sender));
    lines.push(format!("status: {}", msg.status.label()));
    if !msg.timestamp.is_empty() {
        lines.push(format!("when: {}", msg.timestamp));
    }
    lines.push(String::new());
    lines.push(msg.body.clone());
    lines.join("\n")
}

fn status_marker(s: &SmsStatus) -> &'static str {
    match s {
        SmsStatus::ReceivedUnread => "*",
        SmsStatus::ReceivedRead => " ",
        SmsStatus::StoredUnsent => "d",
        SmsStatus::StoredSent => "s",
        SmsStatus::Unknown => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::MockBatteryDriver;
    use crate::charger::MockChargerDriver;
    use crate::credentials::MockCredentialStore;
    use crate::email::MockSmtpStreamFactory;
    use crate::http::MockHttpClient;
    use crate::modem::{MockModem, Modem, ModemError};
    use crate::saved_networks::MockNetworkStore;
    use crate::wifi::MockWifiDriver;

    struct Env {
        wifi: MockWifiDriver,
        http: MockHttpClient,
        saved: MockNetworkStore,
        smtp: MockSmtpStreamFactory,
        creds: MockCredentialStore,
        modem: MockModem,
        battery: MockBatteryDriver,
        charger: MockChargerDriver,
    }

    impl Env {
        fn new() -> Self {
            let mut env = Self {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
                modem: MockModem::new(),
                battery: MockBatteryDriver::new(),
                charger: MockChargerDriver::new(),
            };
            // SMS commands all require an active modem; default to on so
            // tests don't need to power it explicitly.
            env.modem.power_on().unwrap();
            env
        }
        fn ctx(&mut self) -> ExecContext<'_> {
            ExecContext {
                uptime_secs: 0,
                wifi: &mut self.wifi,
                http: &mut self.http,
                saved_networks: &mut self.saved,
                smtp: &mut self.smtp,
                credentials: &mut self.creds,
                modem: &mut self.modem,
                battery: &mut self.battery,
                charger: &mut self.charger,
            }
        }
    }

    #[test]
    fn no_args_shows_usage() {
        let mut env = Env::new();
        let r = run(&[], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, USAGE);
    }

    #[test]
    fn unknown_subcommand_errors() {
        let mut env = Env::new();
        let r = run(&["nope"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown subcommand"));
    }

    // --- send ----------------------------------------------------------------

    #[test]
    fn send_with_no_body_errors() {
        let mut env = Env::new();
        let r = run(&["send", "+447"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("usage"));
    }

    #[test]
    fn send_when_modem_off_errors_without_calling_modem() {
        let mut env = Env::new();
        env.modem.power_off().unwrap();
        let r = run(&["send", "+447", "hi"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("modem is off"));
    }

    #[test]
    fn send_happy_path_returns_confirmation() {
        let mut env = Env::new();
        env.modem
            .on_with_body("AT+CMGS=\"+447\"", Ok(vec!["+CMGS: 12".into()]));
        let r = run(&["send", "+447", "hello"], &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.contains("sent to +447"));
        assert_eq!(env.modem.body_log[0].1, b"hello\x1a");
    }

    #[test]
    fn send_joins_multiword_body() {
        let mut env = Env::new();
        env.modem
            .on_with_body("AT+CMGS=\"+447\"", Ok(vec!["+CMGS: 1".into()]));
        let r = run(&["send", "+447", "hello", "there"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(env.modem.body_log[0].1, b"hello there\x1a");
    }

    #[test]
    fn send_propagates_cms_error() {
        let mut env = Env::new();
        env.modem
            .on_with_body("AT+CMGS=\"+447\"", Err(ModemError::CmsError(310)));
        let r = run(&["send", "+447", "hi"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("send failed"));
        assert!(r.output.contains("CMS ERROR 310"));
    }

    // --- inbox ---------------------------------------------------------------

    #[test]
    fn inbox_when_empty_says_so() {
        let mut env = Env::new();
        env.modem.on_raw("AT+CMGL=\"ALL\"", Ok(vec![]));
        let r = run(&["inbox"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "(no messages)");
    }

    #[test]
    fn inbox_lists_messages() {
        let mut env = Env::new();
        env.modem.on_raw(
            "AT+CMGL=\"ALL\"",
            Ok(vec![
                "+CMGL: 1,\"REC UNREAD\",\"+447\",,\"23/04/15,14:30:00+04\"".to_string(),
                "first message".to_string(),
                "+CMGL: 2,\"REC READ\",\"+447\",,\"23/04/16,09:15:32+00\"".to_string(),
                "second message".to_string(),
            ]),
        );
        let r = run(&["inbox"], &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.contains("[1] *"));
        assert!(r.output.contains("first message"));
        assert!(r.output.contains("[2]  "));
        assert!(r.output.contains("second message"));
    }

    #[test]
    fn inbox_when_modem_off_errors() {
        let mut env = Env::new();
        env.modem.power_off().unwrap();
        let r = run(&["inbox"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("modem is off"));
    }

    // --- read ----------------------------------------------------------------

    #[test]
    fn read_requires_index() {
        let mut env = Env::new();
        let r = run(&["read"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn read_invalid_index_errors() {
        let mut env = Env::new();
        let r = run(&["read", "abc"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("invalid index"));
    }

    #[test]
    fn read_returns_formatted_message() {
        let mut env = Env::new();
        env.modem.on_raw(
            "AT+CMGR=3",
            Ok(vec![
                "+CMGR: \"REC READ\",\"+447\",,\"23/04/16,09:15:32+00\"".to_string(),
                "the body".to_string(),
            ]),
        );
        let r = run(&["read", "3"], &mut env.ctx());
        assert_eq!(r.exit_code, 0, "output: {}", r.output);
        assert!(r.output.contains("from: +447"));
        assert!(r.output.contains("status: read"));
        assert!(r.output.contains("when: 23/04/16,09:15:32+00"));
        assert!(r.output.contains("the body"));
    }

    #[test]
    fn read_not_found_errors() {
        let mut env = Env::new();
        env.modem
            .on_raw("AT+CMGR=99", Err(ModemError::CmsError(321)));
        let r = run(&["read", "99"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("99 not found"));
    }

    // --- delete --------------------------------------------------------------

    #[test]
    fn delete_requires_index() {
        let mut env = Env::new();
        let r = run(&["delete"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
    }

    #[test]
    fn delete_happy_path() {
        let mut env = Env::new();
        let r = run(&["delete", "5"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("deleted message 5"));
    }
}
