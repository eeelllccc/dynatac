//! `modem` — control the 4G LTE modem (power, status, raw AT, data).
//!
//! Subcommands:
//!   - `modem status`     — show powered/SIM/registration/signal
//!   - `modem on`         — power on and wait until responsive
//!   - `modem off`        — power off
//!   - `modem at <cmd>`   — send a raw AT command and print the info lines
//!   - `modem data on`    — bring up a PPP cellular data session (uses the
//!                          hardcoded APN from `crate::network::APN`)
//!   - `modem data off`   — tear down the data session
//!   - `modem data status` — show whether data is currently active
//!
//! The modem is lazy: it does not turn on at boot (the cellular radio is
//! the most power-hungry thing on the device). A user must explicitly
//! `modem on` before any data- or SMS-dependent command can use it.
//!
//! `modem data on` is the manual equivalent of what `ensure_connectivity`
//! does automatically when WiFi is disconnected and a program (curl,
//! email) needs network access. Useful for testing PPP in isolation.

use crate::modem::{ModemStatus, RegistrationStatus, SimStatus};
use crate::network::APN;

use super::{ExecContext, ProgramResult};

pub const USAGE: &str = "modem [status|on|off|at <cmd>|data on|data off|data status]";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("status") => status(ctx),
        Some("on") => power_on(ctx),
        Some("off") => power_off(ctx),
        Some("at") => raw(&args[1..], ctx),
        Some("data") => data(&args[1..], ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
        None => ProgramResult::ok(USAGE.to_string()),
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

fn data(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("on") => data_on(ctx),
        Some("off") => data_off(ctx),
        Some("status") | None => data_status(ctx),
        Some(other) => ProgramResult::err(format!("unknown data subcommand: {}", other)),
    }
}

fn data_on(ctx: &mut ExecContext) -> ProgramResult {
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off — run `modem on` first".to_string());
    }
    if ctx.modem.is_data_active() {
        return ProgramResult::ok("data already up".to_string());
    }
    match ctx.modem.enable_data(APN) {
        Ok(()) => ProgramResult::ok(format!("data up (APN={})", APN)),
        Err(e) => ProgramResult::err(format!("data up failed: {}", e.display())),
    }
}

fn data_off(ctx: &mut ExecContext) -> ProgramResult {
    if !ctx.modem.is_data_active() {
        return ProgramResult::ok("data already down".to_string());
    }
    match ctx.modem.disable_data() {
        Ok(()) => ProgramResult::ok("data down".to_string()),
        Err(e) => ProgramResult::err(format!("data down failed: {}", e.display())),
    }
}

fn data_status(ctx: &mut ExecContext) -> ProgramResult {
    if ctx.modem.is_data_active() {
        ProgramResult::ok("data: up".to_string())
    } else {
        ProgramResult::ok("data: down".to_string())
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
            Self {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
                modem: MockModem::new(),
                battery: MockBatteryDriver::new(),
                charger: MockChargerDriver::new(),
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

    // --- data subcommand ----------------------------------------------------

    #[test]
    fn data_status_when_down() {
        let mut env = Env::new();
        let r = run(&["data", "status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "data: down");
    }

    #[test]
    fn data_on_when_modem_off_errors() {
        let mut env = Env::new();
        let r = run(&["data", "on"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("modem is off"));
    }

    #[test]
    fn data_on_happy_path() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["data", "on"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("data up"));
        assert!(env.modem.is_data_active());
        assert_eq!(env.modem.last_apn.as_deref(), Some(crate::network::APN));
    }

    #[test]
    fn data_on_when_already_active_is_noop() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.enable_data(crate::network::APN).unwrap();
        let r = run(&["data", "on"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("already up"));
    }

    #[test]
    fn data_off_happy_path() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.enable_data(crate::network::APN).unwrap();
        let r = run(&["data", "off"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "data down");
        assert!(!env.modem.is_data_active());
    }

    #[test]
    fn data_off_when_already_down_is_noop() {
        let mut env = Env::new();
        let r = run(&["data", "off"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("already down"));
    }

    #[test]
    fn data_on_propagates_modem_error() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.enable_data_error = Some(crate::modem::ModemError::Timeout);
        let r = run(&["data", "on"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("data up failed"));
        assert!(r.output.contains("timeout"));
    }

    #[test]
    fn data_unknown_subcommand_errors() {
        let mut env = Env::new();
        let r = run(&["data", "nope"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown data subcommand"));
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
