//! `echo` — print arguments separated by spaces.

use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], _ctx: &mut ExecContext) -> ProgramResult {
    ProgramResult::ok(args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::http::MockHttpClient;
    use crate::saved_networks::MockNetworkStore;
    use crate::wifi::MockWifiDriver;

    #[test]
    fn no_args_prints_empty() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = ExecContext { uptime_secs: 0, wifi: &mut wifi, http: &mut http, saved_networks: &mut saved };
        let r = run(&[], &mut ctx);
        assert_eq!(r.output, "");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn single_arg() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = ExecContext { uptime_secs: 0, wifi: &mut wifi, http: &mut http, saved_networks: &mut saved };
        let r = run(&["hello"], &mut ctx);
        assert_eq!(r.output, "hello");
    }

    #[test]
    fn multiple_args_joined_with_spaces() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = ExecContext { uptime_secs: 0, wifi: &mut wifi, http: &mut http, saved_networks: &mut saved };
        let r = run(&["hello", "world"], &mut ctx);
        assert_eq!(r.output, "hello world");
    }
}
