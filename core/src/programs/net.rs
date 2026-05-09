//! `net` — show which transport is currently serving outgoing
//! network traffic.
//!
//! Subcommands:
//!   - `net status` (or bare `net`) — one-line current transport
//!
//! This command is **purely observational**: it inspects `ctx.wifi` and
//! `ctx.modem` state without triggering any bring-up or teardown. That's
//! different from what `curl` and `email` do, which call
//! `ensure_connectivity` and may cause a transport to change.
//!
//! The output mirrors the policy in [`crate::network::ensure_connectivity`]:
//!   - wifi connected           → `transport: wifi (<ssid>)`
//!   - wifi down + data active  → `transport: cellular (APN: <apn>)`
//!   - wifi down + no data      → `transport: none`

use crate::network::APN;
use crate::wifi::WifiStatus;

use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("status") | None => status(ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
    }
}

fn status(ctx: &mut ExecContext) -> ProgramResult {
    let wifi_status = ctx.wifi.status();
    let cellular_active = ctx.modem.is_data_active();

    let line = match (&wifi_status, cellular_active) {
        // WiFi wins even if a cellular session happens to still be up
        // (i.e. we haven't had a chance to tear it down yet) — this
        // matches ensure_connectivity's policy.
        (WifiStatus::Connected(ssid), _) => format!("transport: wifi ({})", ssid),
        (WifiStatus::Disconnected, true) => format!("transport: cellular (APN: {})", APN),
        (WifiStatus::Disconnected, false) => "transport: none".to_string(),
    };
    ProgramResult::ok(line)
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
    use crate::wifi::{MockWifiDriver, WifiDriver};

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
    fn bare_net_defaults_to_status() {
        let mut env = Env::new();
        let r = run(&[], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert!(r.output.starts_with("transport: "));
    }

    #[test]
    fn unknown_subcommand_errors() {
        let mut env = Env::new();
        let r = run(&["nope"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown subcommand"));
    }

    #[test]
    fn reports_wifi_with_ssid() {
        let mut env = Env::new();
        env.wifi.connect("home_wifi", "").unwrap();
        let r = run(&["status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "transport: wifi (home_wifi)");
    }

    #[test]
    fn reports_cellular_when_wifi_down_and_data_up() {
        let mut env = Env::new();
        env.modem.power_on().unwrap();
        env.modem.enable_data(APN).unwrap();
        let r = run(&["status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, format!("transport: cellular (APN: {})", APN));
    }

    #[test]
    fn reports_none_when_nothing_up() {
        let mut env = Env::new();
        let r = run(&["status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "transport: none");
    }

    #[test]
    fn wifi_beats_stale_cellular_session() {
        // If both wifi is connected AND cellular is somehow still up
        // (e.g. we haven't called ensure_connectivity yet to trigger
        // the auto-teardown), we still report wifi — that's what the
        // next network call will use.
        let mut env = Env::new();
        env.wifi.connect("home_wifi", "").unwrap();
        env.modem.power_on().unwrap();
        env.modem.enable_data(APN).unwrap();
        let r = run(&["status"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "transport: wifi (home_wifi)");
    }

    #[test]
    fn status_is_purely_observational() {
        // Running `net status` must not bring up or tear down anything.
        // Mock wifi starts disconnected by default, which is the
        // interesting state here (maximum temptation to bring up cellular).
        let mut env = Env::new();
        let powered_before = env.modem.is_powered();
        let data_before = env.modem.is_data_active();
        run(&["status"], &mut env.ctx());
        assert_eq!(env.modem.is_powered(), powered_before);
        assert_eq!(env.modem.is_data_active(), data_before);
    }
}
