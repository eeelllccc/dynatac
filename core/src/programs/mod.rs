//! CLI programs that can be invoked from the shell.
//!
//! Each program is a function `fn(&[&str], &mut ExecContext) -> ProgramResult`.
//! Programs live in their own submodules and are registered in [`PROGRAMS`].

use crate::battery::BatteryDriver;
use crate::charger::ChargerDriver;
use crate::credentials::CredentialStore;
use crate::email::SmtpStreamFactory;
use crate::http::HttpClient;
use crate::modem::Modem;
use crate::saved_networks::NetworkStore;
use crate::wifi::WifiDriver;

pub mod battery;
pub mod curl;
pub mod echo;
pub mod email;
pub mod modem;
pub mod net;
pub mod power;
pub mod sms;
pub mod whatsapp;
pub mod wifi;

/// Ambient state available to every program (clock, drivers, etc.).
pub struct ExecContext<'a> {
    pub uptime_secs: u64,
    pub wifi: &'a mut dyn WifiDriver,
    pub http: &'a mut dyn HttpClient,
    pub saved_networks: &'a mut dyn NetworkStore,
    pub smtp: &'a mut dyn SmtpStreamFactory,
    pub credentials: &'a mut dyn CredentialStore,
    pub modem: &'a mut dyn Modem,
    pub battery: &'a mut dyn BatteryDriver,
    pub charger: &'a mut dyn ChargerDriver,
}

/// The result of running a program.
pub struct ProgramResult {
    pub output: String,
    pub exit_code: i32,
}

impl ProgramResult {
    pub fn ok(output: String) -> Self {
        Self { output, exit_code: 0 }
    }

    pub fn err(output: String) -> Self {
        Self { output, exit_code: 1 }
    }
}

/// Signature shared by all program entry-points.
pub type ProgramFn = fn(args: &[&str], ctx: &mut ExecContext) -> ProgramResult;

/// Called when the user selects an item from an interactive list.
/// `context` carries state from the command that started the list (e.g. "connect" vs "forget").
pub type OnListSelectFn = fn(context: &str, selected: &str, ctx: &mut ExecContext) -> ProgramResult;

/// Called when the user submits text in an interactive prompt.
/// `context` carries state from the prior step (e.g. selected SSID).
pub type OnTextSubmitFn = fn(context: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult;

/// A program registered in the shell.
pub struct Program {
    pub name: &'static str,
    /// One-line summary shown in `help`. For programs where `usage_on_empty`
    /// is true, this must exactly match what `run(&[], ctx)` returns — enforced
    /// by the `no_args_returns_registry_usage` test. Export the string as
    /// `pub const USAGE` from the program module and reference it here so
    /// there is a single source of truth.
    pub usage: &'static str,
    pub run: ProgramFn,
    /// If this program can trigger a list selection, this handles the result.
    pub on_list_select: Option<OnListSelectFn>,
    /// If this program can trigger a text prompt, this handles the submitted text.
    pub on_text_submit: Option<OnTextSubmitFn>,
    /// True when `run(&[], ctx)` returns `ProgramResult::ok(usage)`.
    /// Programs that do something else on no-args (e.g. `echo` prints empty,
    /// `net` runs status, `curl`/`email` error) set this to false.
    pub usage_on_empty: bool,
}

/// All available programs. The shell searches this list by name.
pub static PROGRAMS: &[Program] = &[
    Program {
        name: "battery",
        usage: battery::USAGE,
        run: battery::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: true,
    },
    Program {
        name: "curl",
        usage: "curl <url>",
        run: curl::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: false, // no-args is an error (URL required)
    },
    Program {
        name: "echo",
        usage: "echo [args...]",
        run: echo::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: false, // no-args prints empty (correct behavior)
    },
    Program {
        name: "email",
        usage: "email <to> \"<subject>\" \"<body>\" | email setup",
        run: email::run,
        on_list_select: None,
        on_text_submit: Some(email::on_text_submit),
        usage_on_empty: false, // no-args is an error (args required)
    },
    Program {
        name: "modem",
        usage: modem::USAGE,
        run: modem::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: true,
    },
    Program {
        name: "net",
        usage: "net [status]",
        run: net::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: false, // no-args runs status, not usage
    },
    Program {
        name: "power",
        usage: power::USAGE,
        run: power::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: true,
    },
    Program {
        name: "sms",
        usage: sms::USAGE,
        run: sms::run,
        on_list_select: None,
        on_text_submit: None,
        usage_on_empty: true,
    },
    Program {
        name: "whatsapp",
        usage: whatsapp::USAGE,
        run: whatsapp::run,
        on_list_select: Some(whatsapp::on_list_select),
        on_text_submit: Some(whatsapp::on_text_submit),
        usage_on_empty: true,
    },
    Program {
        name: "wifi",
        usage: wifi::USAGE,
        run: wifi::run,
        on_list_select: Some(wifi::on_list_select),
        on_text_submit: Some(wifi::on_text_submit),
        usage_on_empty: true,
    },
];

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

    #[test]
    fn no_args_returns_registry_usage() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut smtp = MockSmtpStreamFactory::new();
        let mut creds = MockCredentialStore::new();
        let mut modem = MockModem::new();
        let mut battery = MockBatteryDriver::new();
        let mut charger = MockChargerDriver::new();
        let mut ctx = ExecContext {
            uptime_secs: 0,
            wifi: &mut wifi,
            http: &mut http,
            saved_networks: &mut saved,
            smtp: &mut smtp,
            credentials: &mut creds,
            modem: &mut modem,
            battery: &mut battery,
            charger: &mut charger,
        };
        for program in PROGRAMS {
            if !program.usage_on_empty {
                continue;
            }
            let result = (program.run)(&[], &mut ctx);
            assert_eq!(
                result.exit_code, 0,
                "program '{}': no-args exit_code should be 0",
                program.name
            );
            assert_eq!(
                result.output, program.usage,
                "program '{}': no-args output != registry usage\n\
                 update the program's pub const USAGE or the registry entry so they match",
                program.name
            );
        }
    }
}
