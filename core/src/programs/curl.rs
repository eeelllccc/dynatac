//! `curl` — fetch a URL via HTTP GET and print the response body.
//!
//! Transport-agnostic: the program doesn't know or care whether it's
//! running over wifi or cellular. `ensure_connectivity` decides, and
//! its transition logs tell the user when fallback happens. Program
//! output is the HTTP response body verbatim, regardless of transport.

use super::{ExecContext, ProgramResult};
use crate::network::{ensure_connectivity, APN};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    let url = match args.first() {
        Some(u) => *u,
        None => return ProgramResult::err("usage: curl <url>".to_string()),
    };
    // Make sure *some* IP transport is live. The first cellular
    // fallback can take 10–20 s while the modem dials in; subsequent
    // calls within the same session are fast.
    if let Err(e) = ensure_connectivity(ctx.wifi, ctx.modem, APN) {
        return ProgramResult::err(format!("no connectivity: {}", e.display()));
    }
    match ctx.http.get(url) {
        Ok(body) => ProgramResult::ok(body),
        Err(e) => ProgramResult::err(format!("error: {}", e)),
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
        /// Builds an Env with WiFi *connected* by default — the normal
        /// operating state we expect for most tests. Tests that want to
        /// exercise the cellular fallback path call `wifi.disconnect()`
        /// explicitly.
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
            env.wifi.connect("home_wifi", "").unwrap();
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
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("usage"));
    }

    #[test]
    fn successful_get_on_wifi() {
        let mut env = Env::new();
        env.http
            .on_get("http://example.com", Ok("<html>hello</html>".into()));
        let r = run(&["http://example.com"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "<html>hello</html>");
        // WiFi was connected from the start; modem was never touched.
        assert!(!env.modem.is_powered());
    }

    #[test]
    fn falls_back_to_cellular_when_wifi_down() {
        let mut env = Env::new();
        env.wifi.disconnect().unwrap();
        env.http
            .on_get("http://example.com", Ok("<html>hello</html>".into()));
        let r = run(&["http://example.com"], &mut env.ctx());
        // Output must be byte-identical to the wifi path — programs
        // are transport-agnostic now.
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "<html>hello</html>");
        // Verify the fallback actually happened by inspecting modem
        // state directly, not the output string.
        assert!(env.modem.is_powered());
        assert!(env.modem.is_data_active());
    }

    #[test]
    fn failed_get() {
        let mut env = Env::new();
        env.http
            .on_get("http://fail.com", Err("connection refused".into()));
        let r = run(&["http://fail.com"], &mut env.ctx());
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("connection refused"));
    }
}
