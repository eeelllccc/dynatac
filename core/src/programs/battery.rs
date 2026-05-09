//! `battery` — report state of charge and charging status.
//!
//! Subcommands:
//!   - `battery level`    — SOC as a percentage, e.g. `87%`
//!   - `battery charging` — one of `charging` / `discharging` / `full`
//!
//! Both subcommands perform exactly one I2C transaction against the
//! fuel gauge via [`ctx.battery`]. This is **purely observational** —
//! no writes to the gauge, no side effects beyond the bus read.

use crate::battery::{BatteryError, ChargeState};

use super::{ExecContext, ProgramResult};

pub const USAGE: &str = "battery [level|charging]";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        None => ProgramResult::ok(USAGE.into()),
        Some("level") => level(ctx),
        Some("charging") => charging(ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
    }
}

fn level(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.battery.level() {
        Ok(pct) => ProgramResult::ok(format!("{}%", pct)),
        Err(BatteryError::Bus) => ProgramResult::err("battery: I2C read failed".into()),
    }
}

fn charging(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.battery.charge_state() {
        Ok(ChargeState::Charging) => ProgramResult::ok("charging".into()),
        Ok(ChargeState::Discharging) => ProgramResult::ok("discharging".into()),
        Ok(ChargeState::Full) => ProgramResult::ok("full".into()),
        Err(BatteryError::Bus) => ProgramResult::err("battery: I2C read failed".into()),
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
    use crate::modem::MockModem;
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
    fn unknown_subcommand_errors() {
        let mut env = Env::new();
        let r = run(&["nope"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown subcommand"));
    }

    #[test]
    fn level_reports_percent() {
        let mut env = Env::new();
        env.battery.level = Ok(42);
        let r = run(&["level"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "42%");
    }

    #[test]
    fn level_reports_zero() {
        let mut env = Env::new();
        env.battery.level = Ok(0);
        let r = run(&["level"], &mut env.ctx());
        assert_eq!(r.output, "0%");
    }

    #[test]
    fn level_reports_full() {
        let mut env = Env::new();
        env.battery.level = Ok(100);
        let r = run(&["level"], &mut env.ctx());
        assert_eq!(r.output, "100%");
    }

    #[test]
    fn level_reports_bus_error() {
        let mut env = Env::new();
        env.battery.level = Err(BatteryError::Bus);
        let r = run(&["level"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("I2C"));
    }

    #[test]
    fn charging_state_charging() {
        let mut env = Env::new();
        env.battery.charge_state = Ok(ChargeState::Charging);
        let r = run(&["charging"], &mut env.ctx());
        assert_eq!(r.output, "charging");
    }

    #[test]
    fn charging_state_discharging() {
        let mut env = Env::new();
        env.battery.charge_state = Ok(ChargeState::Discharging);
        let r = run(&["charging"], &mut env.ctx());
        assert_eq!(r.output, "discharging");
    }

    #[test]
    fn charging_state_full() {
        let mut env = Env::new();
        env.battery.charge_state = Ok(ChargeState::Full);
        let r = run(&["charging"], &mut env.ctx());
        assert_eq!(r.output, "full");
    }

    #[test]
    fn charging_reports_bus_error() {
        let mut env = Env::new();
        env.battery.charge_state = Err(BatteryError::Bus);
        let r = run(&["charging"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("I2C"));
    }
}
