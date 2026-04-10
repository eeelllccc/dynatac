//! `modem` — control the 4G LTE modem (power, status, raw AT).
//!
//! Subcommands:
//!   - `modem status`    — show powered/SIM/registration/signal
//!   - `modem on`        — power on and wait until responsive
//!   - `modem off`       — power off
//!   - `modem at <cmd>`  — send a raw AT command and print the info lines
//!
//! The modem is lazy: it does not turn on at boot (the cellular radio is
//! the most power-hungry thing on the device). A user must explicitly
//! `modem on` before any data- or SMS-dependent command can use it.

use crate::modem::{ModemStatus, RegistrationStatus, SimStatus};

use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("status") => status(ctx),
        Some("on") => power_on(ctx),
        Some("off") => power_off(ctx),
        Some("at") => raw(&args[1..], ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
        None => ProgramResult::ok("usage: modem [status|on|off|at <cmd>]".to_string()),
    }
}

fn status(ctx: &mut ExecContext) -> ProgramResult {
    if !ctx.modem.is_powered() {
        return ProgramResult::ok("powered: no".to_string());
    }
    match ctx.modem.status() {
        Ok(s) => ProgramResult::ok(format_status(&s)),
        Err(e) => ProgramResult::err(e.display()),
    }
}

fn power_on(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.modem.power_on() {
        Ok(()) => ProgramResult::ok("modem on".to_string()),
        Err(e) => ProgramResult::err(format!("power on failed: {}", e.display())),
    }
}

fn power_off(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.modem.power_off() {
        Ok(()) => ProgramResult::ok("modem off".to_string()),
        Err(e) => ProgramResult::err(format!("power off failed: {}", e.display())),
    }
}

fn raw(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    if args.is_empty() {
        return ProgramResult::err("usage: modem at <cmd>".to_string());
    }
    // Re-join so users can say `modem at AT+CSQ` or `modem at AT+CMGF=1`.
    let cmd = args.join(" ");
    match ctx.modem.send_raw(&cmd) {
        Ok(lines) if lines.is_empty() => ProgramResult::ok("OK".to_string()),
        Ok(lines) => {
            let mut out = lines.join("\n");
            out.push_str("\nOK");
            ProgramResult::ok(out)
        }
        Err(e) => ProgramResult::err(e.display()),
    }
}

fn format_status(s: &ModemStatus) -> String {
    let mut lines = Vec::new();
    lines.push("powered: yes".to_string());
    lines.push(format!(
        "responsive: {}",
        if s.responsive { "yes" } else { "no" }
    ));
    lines.push(format!("sim: {}", sim_text(&s.sim)));
    lines.push(format!("network: {}", reg_text(&s.registration)));
    let signal = match s.signal_dbm {
        Some(dbm) => format!("{} dBm", dbm),
        None => "unknown".to_string(),
    };
    lines.push(format!("signal: {}", signal));
    lines.join("\n")
}

fn sim_text(s: &SimStatus) -> &'static str {
    match s {
        SimStatus::Ready => "ready",
        SimStatus::Locked => "locked (PIN required)",
        SimStatus::NotReady => "not ready",
        SimStatus::Unknown => "unknown",
    }
}

fn reg_text(r: &RegistrationStatus) -> &'static str {
    match r {
        RegistrationStatus::NotRegistered => "not registered",
        RegistrationStatus::RegisteredHome => "registered (home)",
        RegistrationStatus::Searching => "searching",
        RegistrationStatus::Denied => "denied",
        RegistrationStatus::Roaming => "registered (roaming)",
        RegistrationStatus::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }

    impl Env {
        fn new() -> Self {
            Self {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
                modem: MockModem::new(),
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
                modem: &mut self.modem,
            }
        }
    }

    #[test]
    fn no_args_shows_usage() {
        let mut env = Env::new();
        let r = run(&[], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("usage"));
    }

    #[test]
    fn unknown_subcommand() {
        let mut env = Env::new();
        let r = run(&["nope"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown subcommand"));
    }

    #[test]
    fn status_when_off_reports_powered_no() {
        let mut env = Env::new();
        let r = run(&["status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "powered: no");
    }

    #[test]
    fn on_then_status_shows_details() {
        let mut env = Env::new();
        run(&["on"], &mut env.ctx());
        let r = run(&["status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("powered: yes"));
        assert!(r.output.contains("responsive: yes"));
        assert!(r.output.contains("sim: ready"));
        assert!(r.output.contains("network: registered (home)"));
        assert!(r.output.contains("signal: -71 dBm"));
    }

    #[test]
    fn on_reports_success() {
        let mut env = Env::new();
        let r = run(&["on"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "modem on");
        assert!(env.modem.is_powered());
    }

    #[test]
    fn off_reports_success() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["off"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "modem off");
        assert!(!env.modem.is_powered());
    }

    #[test]
    fn at_without_args_errors() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["at"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("usage"));
    }

    #[test]
    fn at_when_off_reports_not_powered() {
        let mut env = Env::new();
        let r = run(&["at", "AT"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.output, "modem is off");
    }

    #[test]
    fn at_empty_response_shows_ok() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["at", "AT"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "OK");
    }

    #[test]
    fn at_with_info_lines_appends_ok() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem
            .on_raw("AT+CSQ", Ok(vec!["+CSQ: 20,99".to_string()]));
        let r = run(&["at", "AT+CSQ"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "+CSQ: 20,99\nOK");
    }

    #[test]
    fn at_joins_multiword_command() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.on_raw(
            "AT+CMGF=1",
            Ok(vec![]),
        );
        let r = run(&["at", "AT+CMGF=1"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "OK");
    }

    #[test]
    fn at_propagates_cme_error() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem
            .on_raw("AT+CPIN?", Err(ModemError::CmeError(10)));
        let r = run(&["at", "AT+CPIN?"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.output, "CME ERROR 10");
    }
}
