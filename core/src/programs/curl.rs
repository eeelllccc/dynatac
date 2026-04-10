//! `curl` — fetch a URL via HTTP GET and print the response body.

use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    let url = match args.first() {
        Some(u) => *u,
        None => return ProgramResult::err("usage: curl <url>".to_string()),
    };
    match ctx.http.get(url) {
        Ok(body) => ProgramResult::ok(body),
        Err(e) => ProgramResult::err(format!("error: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::MockCredentialStore;
    use crate::email::MockSmtpStreamFactory;
    use crate::http::MockHttpClient;
    use crate::saved_networks::MockNetworkStore;
    use crate::wifi::MockWifiDriver;

    struct Env {
        wifi: MockWifiDriver,
        http: MockHttpClient,
        saved: MockNetworkStore,
        smtp: MockSmtpStreamFactory,
        creds: MockCredentialStore,
    }

    impl Env {
        fn new() -> Self {
            Self {
                wifi: MockWifiDriver::new(),
                http: MockHttpClient::new(),
                saved: MockNetworkStore::new(),
                smtp: MockSmtpStreamFactory::new(),
                creds: MockCredentialStore::new(),
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
    fn successful_get() {
        let mut env = Env::new();
        env.http
            .on_get("http://example.com", Ok("<html>hello</html>".into()));
        let r = run(&["http://example.com"], &mut env.ctx());
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "<html>hello</html>");
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
