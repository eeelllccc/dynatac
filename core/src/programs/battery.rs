//! `battery` — report state of charge and charging status.
//!
//! Subcommands:
//!   - `battery level`    — SOC as a percentage, e.g. `87%`
//!   - `battery charging` — one of `charging` / `discharging` / `full`
//!
//! Both subcommands perform exactly one I2C transaction against the
//! fuel gauge via [`ctx.battery`]. This is **purely observational** —
//! no writes to the gauge, no side effects beyond the bus read.

use crate::battery::{BatteryDiag, BatteryError, ChargeState, CEDV_PROFILE_1400MAH};

use super::{ExecContext, ProgramResult};

pub const USAGE: &str = "battery [level|charging|diag|init]";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        None => ProgramResult::ok(USAGE.into()),
        Some("level") => level(ctx),
        Some("charging") => charging(ctx),
        Some("diag") => diag(ctx),
        Some("init") => init(ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
    }
}

fn level(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.battery.level() {
        Ok(pct) => ProgramResult::ok(format!("{}%", pct)),
        Err(_) => ProgramResult::err("battery: I2C read failed".into()),
    }
}

fn charging(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.battery.charge_state() {
        Ok(ChargeState::Charging) => ProgramResult::ok("charging".into()),
        Ok(ChargeState::Discharging) => ProgramResult::ok("discharging".into()),
        Ok(ChargeState::Full) => ProgramResult::ok("full".into()),
        Err(_) => ProgramResult::err("battery: I2C read failed".into()),
    }
}

fn diag(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.battery.diag() {
        Ok(d) => ProgramResult::ok(format_diag(&d)),
        Err(_) => ProgramResult::err("battery: I2C read failed".into()),
    }
}

fn init(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.battery.provision() {
        Ok(()) => ProgramResult::ok(format!(
            "BQ27220 provisioned: {} parameters written",
            CEDV_PROFILE_1400MAH.len()
        )),
        Err(BatteryError::Timeout) => {
            ProgramResult::err("battery: gauge timed out during provisioning".into())
        }
        Err(BatteryError::Bus) => {
            ProgramResult::err("battery: I2C error during provisioning".into())
        }
    }
}

fn format_diag(d: &BatteryDiag) -> String {
    let sign = if d.current_ma >= 0 { "+" } else { "" };
    format!(
        "initcomp:   {}\ndesign_cap: {} mAh\nfull_cap:   {} mAh\nremaining:  {} mAh\nvoltage:    {} mV\ncurrent:    {}{} mA",
        if d.initcomp { "yes" } else { "no (SOC unreliable)" },
        d.design_capacity_mah,
        d.full_charge_capacity_mah,
        d.remaining_capacity_mah,
        d.voltage_mv,
        sign,
        d.current_ma,
    )
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

    #[test]
    fn diag_shows_all_fields() {
        use crate::battery::BatteryDiag;
        let mut env = Env::new();
        env.battery.diag = Ok(BatteryDiag {
            initcomp: false,
            cfg_update: false,
            design_capacity_mah: 1000,
            full_charge_capacity_mah: 900,
            remaining_capacity_mah: 550,
            voltage_mv: 3765,
            current_ma: 312,
        });
        let r = run(&["diag"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("no (SOC unreliable)"));
        assert!(r.output.contains("1000 mAh"));
        assert!(r.output.contains("900 mAh"));
        assert!(r.output.contains("550 mAh"));
        assert!(r.output.contains("3765 mV"));
        assert!(r.output.contains("+312 mA"));
    }

    #[test]
    fn diag_initcomp_yes() {
        let mut env = Env::new();
        let r = run(&["diag"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("initcomp:   yes"));
    }

    #[test]
    fn diag_negative_current() {
        use crate::battery::BatteryDiag;
        let mut env = Env::new();
        env.battery.diag = Ok(BatteryDiag {
            initcomp: true,
            cfg_update: false,
            design_capacity_mah: 1400,
            full_charge_capacity_mah: 1400,
            remaining_capacity_mah: 700,
            voltage_mv: 3700,
            current_ma: -150,
        });
        let r = run(&["diag"], &mut env.ctx());
        assert!(r.output.contains("-150 mA"));
    }

    #[test]
    fn diag_reports_bus_error() {
        let mut env = Env::new();
        env.battery.diag = Err(BatteryError::Bus);
        let r = run(&["diag"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("I2C"));
    }

    #[test]
    fn init_success() {
        let mut env = Env::new();
        let r = run(&["init"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("provisioned"));
    }

    #[test]
    fn init_bus_error() {
        let mut env = Env::new();
        env.battery.provision_result = Err(BatteryError::Bus);
        let r = run(&["init"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("I2C"));
    }

    #[test]
    fn init_timeout_error() {
        let mut env = Env::new();
        env.battery.provision_result = Err(BatteryError::Timeout);
        let r = run(&["init"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("timed out"));
    }
}
