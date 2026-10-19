//! `phone` — make a voice call via the 4G modem.
//!
//! Usage:
//!   phone <number>   — dial a voice call (modem must be powered on)
//!   phone hangup     — hang up any active call

use super::{ExecContext, ProgramResult};
use crate::call::Volume;

pub const USAGE: &str = "phone <number> | phone hangup";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("hangup") => hangup(ctx),
        Some(number) => dial(number, ctx),
        None => ProgramResult::ok(USAGE.to_string()),
    }
}

fn dial(number: &str, ctx: &mut ExecContext) -> ProgramResult {
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off — run `modem on` first".to_string());
    }
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit() || c == '+') {
        return ProgramResult::err(format!("invalid number: {}", number));
    }
    // AT+CVHU=0: make ATH terminate voice calls. A7682E default may be CVHU=1
    // which silently ignores ATH for voice calls, leaving the remote connected.
    let _ = ctx.modem.send_raw("AT+CVHU=0");
    // Calls start at max volume (see `call`). A failure here is ignored —
    // a quiet call beats no call; the in-call status line will show it.
    let _ = ctx.modem.send_raw(&Volume::max().set_command());
    // Semicolon suffix = voice call: modem stays in command mode after dialling.
    let cmd = format!("ATD{};", number);
    match ctx.modem.send_raw(&cmd) {
        Ok(_) => ProgramResult::ok(format!("__CALL_ACTIVE__\n{}", number)),
        Err(e) => ProgramResult::err(format!("dial failed: {}", e.display())),
    }
}

fn hangup(ctx: &mut ExecContext) -> ProgramResult {
    if !ctx.modem.is_powered() {
        return ProgramResult::err("modem is off".to_string());
    }
    let _ = ctx.modem.send_raw("AT+CHUP");
    let _ = ctx.modem.send_raw("ATH");
    ProgramResult::ok("call ended".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::MockBatteryDriver;
    use crate::charger::MockChargerDriver;
    use crate::credentials::MockCredentialStore;
    use crate::email::MockSmtpStreamFactory;
    use crate::http::MockHttpClient;
    use crate::modem::{MockModem, Modem};
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
    fn dial_when_modem_off_errors() {
        let mut env = Env::new();
        let r = run(&["07700900123"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("modem is off"));
    }

    #[test]
    fn dial_invalid_number_errors() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["not_a_number"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("invalid number"));
    }

    #[test]
    fn dial_sends_cvhu_and_atd_returns_call_active_signal() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["07700900123"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.starts_with("__CALL_ACTIVE__\n"));
        assert!(r.output.contains("07700900123"));
        assert!(env.modem.raw_log.iter().any(|c| c == "AT+CVHU=0"));
        assert!(env.modem.raw_log.iter().any(|c| c == "ATD07700900123;"));
    }

    #[test]
    fn dial_sets_max_output_gain_before_dialling() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        run(&["07700900123"], &mut env.ctx());
        let log = &env.modem.raw_log;
        let gain = log.iter().position(|c| c == "AT+COUTGAIN=7").expect("gain not set");
        let atd = log.iter().position(|c| c == "ATD07700900123;").expect("not dialled");
        assert!(gain < atd);
    }

    #[test]
    fn dial_proceeds_when_gain_command_fails() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.on_raw("AT+COUTGAIN=7", Err(crate::modem::ModemError::Error));
        let r = run(&["07700900123"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(env.modem.raw_log.iter().any(|c| c == "ATD07700900123;"));
    }

    #[test]
    fn dial_international_number_with_plus() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["+447700900123"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("+447700900123"));
        assert!(env.modem.raw_log.iter().any(|c| c == "ATD+447700900123;"));
    }

    #[test]
    fn dial_modem_error_propagates() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.on_raw("ATD07700900123;", Err(crate::modem::ModemError::Error));
        let r = run(&["07700900123"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("dial failed"));
    }

    #[test]
    fn hangup_when_modem_off_errors() {
        let mut env = Env::new();
        let r = run(&["hangup"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("modem is off"));
    }

    #[test]
    fn hangup_sends_chup_and_ath() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        let r = run(&["hangup"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "call ended");
        assert!(env.modem.raw_log.iter().any(|c| c == "AT+CHUP"));
        assert!(env.modem.raw_log.iter().any(|c| c == "ATH"));
    }
}
