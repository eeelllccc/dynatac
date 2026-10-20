//! `echo` — print arguments separated by spaces.

use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], _ctx: &mut ExecContext) -> ProgramResult {
    ProgramResult::ok(args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::battery::tests::MockBatteryDriver;
    use crate::charger::tests::MockChargerDriver;
    use crate::credentials::tests::MockCredentialStore;
    use crate::email::tests::MockSmtpStreamFactory;
    use crate::http::tests::MockHttpClient;
    use crate::modem::tests::MockModem;
    use crate::saved_networks::tests::MockNetworkStore;
    use crate::wifi::tests::MockWifiDriver;

    /// Bundle of mocks needed to construct an [`ExecContext`] in tests.
    /// Each test stack-allocates this and then borrows fields into the ctx.
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
    fn no_args_prints_empty() {
        let mut env = Env::new();
        let r = run(&[], &mut env.ctx());
        assert_eq!(r.output, "");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn single_arg() {
        let mut env = Env::new();
        let r = run(&["hello"], &mut env.ctx());
        assert_eq!(r.output, "hello");
    }

    #[test]
    fn multiple_args_joined_with_spaces() {
        let mut env = Env::new();
        let r = run(&["hello", "world"], &mut env.ctx());
        assert_eq!(r.output, "hello world");
    }
}
