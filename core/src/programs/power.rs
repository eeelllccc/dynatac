//! `power` — power-management commands.
//!
//! Subcommands:
//!   - `power off`          — enter deep sleep; press the physical button to wake.
//!   - `power ship`         — print a warning about ship mode.
//!   - `power ship confirm` — enter BQ25896 ship mode (USB required to restore power).
//!
//! The two ship-mode steps exist so the user must deliberately type the
//! confirmation command — there is no y/n prompt.

use super::{ExecContext, ProgramResult};


/// Sentinel for deep sleep. Intercepted by the shell → [`ShellAction::PowerOff`].
///
/// [`ShellAction::PowerOff`]: crate::shell::ShellAction::PowerOff
pub const USAGE: &str = "power off | power ship [confirm]";

pub const POWER_OFF_SIGNAL: &str = "__POWER_OFF__";

/// Sentinel for BQ25896 ship mode. Intercepted by the shell → [`ShellAction::ShipMode`].
///
/// [`ShellAction::ShipMode`]: crate::shell::ShellAction::ShipMode
pub const SHIP_MODE_SIGNAL: &str = "__SHIP_MODE__";

const SHIP_WARNING: &str =
    "warning: ship mode disconnects the battery.\n\
     to restore power you must plug in a USB cable.\n\
     run 'power ship confirm' to proceed.";

pub fn run(args: &[&str], _ctx: &mut ExecContext) -> ProgramResult {
    match args {
        [] => ProgramResult::ok(USAGE.into()),
        ["off"] => ProgramResult::ok(POWER_OFF_SIGNAL.into()),
        ["ship"] => ProgramResult::ok(SHIP_WARNING.into()),
        ["ship", "confirm"] => ProgramResult::ok(SHIP_MODE_SIGNAL.into()),
        _ => ProgramResult::err(format!("unknown subcommand: {}", args.join(" "))),
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
    fn off_returns_power_off_signal() {
        let mut env = Env::new();
        let r = run(&["off"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, POWER_OFF_SIGNAL);
        assert!(!env.charger.shutdown_called);
    }

    #[test]
    fn ship_without_confirm_shows_warning() {
        let mut env = Env::new();
        let r = run(&["ship"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.contains("USB"));
        assert!(r.output.contains("power ship confirm"));
        assert!(!env.charger.shutdown_called);
    }

    #[test]
    fn ship_confirm_returns_ship_mode_signal() {
        let mut env = Env::new();
        let r = run(&["ship", "confirm"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, SHIP_MODE_SIGNAL);
        assert!(!env.charger.shutdown_called);
    }
}
