//! `curl` — fetch a URL via HTTP GET and print the response body.

use super::{ExecContext, ProgramResult};
use crate::network::{ensure_connectivity, ActiveTransport, APN};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    let url = match args.first() {
        Some(u) => *u,
        None => return ProgramResult::err("usage: curl <url>".to_string()),
    };
    // Make sure *some* IP transport is live (WiFi, or cellular as a
    // fallback). The first cellular fallback can take 10–20 s while the
    // modem dials in; subsequent calls within the same session are fast.
    let transport = match ensure_connectivity(ctx.wifi, ctx.modem, APN) {
        Ok(t) => t,
        Err(e) => return ProgramResult::err(format!("no connectivity: {}", e.display())),
    };
    match ctx.http.get(url) {
        Ok(body) => match transport {
            ActiveTransport::Wifi => ProgramResult::ok(body),
            ActiveTransport::Cellular => {
                // Prepend a small marker so the user knows cellular
                // fallback was used. One line, no extra formatting.
                ProgramResult::ok(format!("(via cellular)\n{}", body))
            }
        },
        Err(e) => ProgramResult::err(format!("error: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(r.exit_code, 0);
        assert!(r.output.starts_with("(via cellular)\n"));
        assert!(r.output.contains("<html>hello</html>"));
        // Modem was brought up as part of the fallback.
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
