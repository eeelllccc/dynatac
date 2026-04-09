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
    use crate::http::MockHttpClient;
    use crate::saved_networks::MockNetworkStore;
    use crate::wifi::MockWifiDriver;

    fn make_ctx<'a>(
        wifi: &'a mut dyn crate::wifi::WifiDriver,
        http: &'a mut dyn crate::http::HttpClient,
        saved: &'a mut dyn crate::saved_networks::NetworkStore,
    ) -> ExecContext<'a> {
        ExecContext {
            uptime_secs: 0,
            wifi,
            http,
            saved_networks: saved,
        }
    }

    #[test]
    fn no_args_shows_usage() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&[], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("usage"));
    }

    #[test]
    fn successful_get() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        http.on_get("http://example.com", Ok("<html>hello</html>".into()));
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["http://example.com"], &mut ctx);
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.output, "<html>hello</html>");
    }

    #[test]
    fn failed_get() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        http.on_get("http://fail.com", Err("connection refused".into()));
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["http://fail.com"], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("connection refused"));
    }
}
