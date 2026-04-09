//! `wifi` — manage WiFi connections.
//!
//! Subcommands:
//!   - `wifi status`     — show current connection state
//!   - `wifi connect`    — list available networks for selection
//!   - `wifi disconnect` — disconnect from current network
//!   - `wifi forget`     — remove saved credentials for a network

use crate::wifi::WifiStatus;
use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("status") => status(ctx),
        Some("connect") => connect(ctx),
        Some("disconnect") => disconnect(ctx),
        Some("forget") => forget(ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
        None => ProgramResult::ok(
            "usage: wifi [status|connect|disconnect|forget]".to_string(),
        ),
    }
}

/// Called by the shell when the user selects an item from a list.
/// `context` distinguishes which subcommand started the list:
///   - "connect" — selected a network to connect to
///   - "forget"  — selected a saved network to forget
pub fn on_list_select(context: &str, selected: &str, ctx: &mut ExecContext) -> ProgramResult {
    match context {
        "connect" => {
            // If we have saved credentials, connect immediately.
            if let Some(password) = ctx.saved_networks.load(selected) {
                match ctx.wifi.connect(selected, &password) {
                    Ok(()) => ProgramResult::ok(format!("connected to {}", selected)),
                    Err(e) => ProgramResult::err(e),
                }
            } else {
                // No saved password — prompt the user.
                ProgramResult::ok(format!(
                    "__START_TEXT_PROMPT__\n{}\npassword:",
                    selected
                ))
            }
        }
        "forget" => {
            ctx.saved_networks.delete(selected);
            ProgramResult::ok(format!("forgot {}", selected))
        }
        _ => ProgramResult::err(format!("unknown list context: {}", context)),
    }
}

/// Called by the shell after the user submits text in the prompt.
/// `context` is the selected SSID; `text` is the password.
pub fn on_text_submit(context: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult {
    match ctx.wifi.connect(context, text) {
        Ok(()) => {
            ctx.saved_networks.save(context, text);
            ProgramResult::ok(format!("connected to {}", context))
        }
        Err(e) => ProgramResult::err(e),
    }
}

fn status(ctx: &mut ExecContext) -> ProgramResult {
    let msg = match ctx.wifi.status() {
        WifiStatus::Connected(name) => format!("connected to {}", name),
        WifiStatus::Disconnected => "not connected".to_string(),
    };
    ProgramResult::ok(msg)
}

fn connect(ctx: &mut ExecContext) -> ProgramResult {
    let networks = ctx.wifi.scan();
    if networks.is_empty() {
        return ProgramResult::err("no networks found".to_string());
    }
    // Format: "__START_LIST__\ncontext\nheader\nitem1\nitem2\n..."
    let mut lines = vec![
        "__START_LIST__".to_string(),
        "connect".to_string(),
        "select network:".to_string(),
    ];
    lines.extend(networks);
    ProgramResult::ok(lines.join("\n"))
}

fn disconnect(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.wifi.disconnect() {
        Ok(()) => ProgramResult::ok("disconnected".to_string()),
        Err(e) => ProgramResult::err(e),
    }
}

fn forget(ctx: &mut ExecContext) -> ProgramResult {
    let saved = ctx.saved_networks.list();
    if saved.is_empty() {
        return ProgramResult::err("no saved networks".to_string());
    }
    let mut lines = vec![
        "__START_LIST__".to_string(),
        "forget".to_string(),
        "forget network:".to_string(),
    ];
    lines.extend(saved);
    ProgramResult::ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::MockHttpClient;
    use crate::saved_networks::{MockNetworkStore, NetworkStore};
    use crate::wifi::{MockWifiDriver, WifiDriver};

    fn make_ctx<'a>(
        wifi: &'a mut dyn crate::wifi::WifiDriver,
        http: &'a mut dyn crate::http::HttpClient,
        saved: &'a mut dyn crate::saved_networks::NetworkStore,
    ) -> ExecContext<'a> {
        ExecContext { uptime_secs: 0, wifi, http, saved_networks: saved }
    }

    #[test]
    fn no_args_shows_usage() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&[], &mut ctx);
        assert!(r.output.contains("usage:"));
    }

    #[test]
    fn unknown_subcommand() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["foo"], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown subcommand"));
    }

    #[test]
    fn status_when_disconnected() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["status"], &mut ctx);
        assert_eq!(r.output, "not connected");
    }

    #[test]
    fn status_when_connected() {
        let mut wifi = MockWifiDriver::new();
        wifi.connect("home_wifi", "").unwrap();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["status"], &mut ctx);
        assert_eq!(r.output, "connected to home_wifi");
    }

    #[test]
    fn connect_returns_start_list_with_context() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["connect"], &mut ctx);
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_LIST__");
        assert_eq!(lines[1], "connect");
        assert_eq!(lines[2], "select network:");
        assert!(r.output.contains("home_wifi"));
    }

    #[test]
    fn disconnect_when_connected() {
        let mut wifi = MockWifiDriver::new();
        wifi.connect("home_wifi", "").unwrap();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["disconnect"], &mut ctx);
        assert_eq!(r.output, "disconnected");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn disconnect_when_not_connected() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["disconnect"], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.output, "not connected");
    }

    // --- on_list_select: connect flow ---

    #[test]
    fn on_list_select_connect_prompts_when_no_saved_password() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = on_list_select("connect", "coffee_shop", &mut ctx);
        assert!(r.output.starts_with("__START_TEXT_PROMPT__"));
        assert!(r.output.contains("coffee_shop"));
        assert!(r.output.contains("password:"));
    }

    #[test]
    fn on_list_select_connect_auto_connects_with_saved_password() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        saved.save("coffee_shop", "secret");
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = on_list_select("connect", "coffee_shop", &mut ctx);
        assert_eq!(r.output, "connected to coffee_shop");
        assert_eq!(r.exit_code, 0);
    }

    // --- on_list_select: forget flow ---

    #[test]
    fn on_list_select_forget_deletes_credentials() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        saved.save("home_wifi", "pass");
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = on_list_select("forget", "home_wifi", &mut ctx);
        assert_eq!(r.output, "forgot home_wifi");
        assert_eq!(ctx.saved_networks.load("home_wifi"), None);
    }

    // --- on_text_submit ---

    #[test]
    fn on_text_submit_connects_and_saves() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = on_text_submit("coffee_shop", "secret", &mut ctx);
        assert_eq!(r.output, "connected to coffee_shop");
        assert_eq!(r.exit_code, 0);
        assert_eq!(ctx.saved_networks.load("coffee_shop"), Some("secret".to_string()));
    }

    #[test]
    fn on_text_submit_does_not_save_on_failure() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = on_text_submit("doesnt_exist", "secret", &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert_eq!(ctx.saved_networks.load("doesnt_exist"), None);
    }

    // --- wifi forget ---

    #[test]
    fn forget_lists_saved_networks() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        saved.save("home_wifi", "pass");
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["forget"], &mut ctx);
        let lines: Vec<&str> = r.output.lines().collect();
        assert_eq!(lines[0], "__START_LIST__");
        assert_eq!(lines[1], "forget");
        assert_eq!(lines[2], "forget network:");
        assert!(r.output.contains("home_wifi"));
    }

    #[test]
    fn forget_with_no_saved_networks_errors() {
        let mut wifi = MockWifiDriver::new();
        let mut http = MockHttpClient::new();
        let mut saved = MockNetworkStore::new();
        let mut ctx = make_ctx(&mut wifi, &mut http, &mut saved);
        let r = run(&["forget"], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.output, "no saved networks");
    }
}
