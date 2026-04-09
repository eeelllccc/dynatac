//! `wifi` — manage WiFi connections.
//!
//! Subcommands:
//!   - `wifi status`     — show current connection state
//!   - `wifi connect`    — list available networks for selection
//!   - `wifi disconnect` — disconnect from current network

use crate::wifi::WifiStatus;
use super::{ExecContext, ProgramResult};

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args.first().copied() {
        Some("status") => status(ctx),
        Some("connect") => connect(ctx),
        Some("disconnect") => disconnect(ctx),
        Some(other) => ProgramResult::err(format!("unknown subcommand: {}", other)),
        None => ProgramResult::ok(
            "usage: wifi [status|connect|disconnect]".to_string(),
        ),
    }
}

/// Called by the shell when the user selects a network from the list.
pub fn on_list_select(selected: &str, ctx: &mut ExecContext) -> ProgramResult {
    match ctx.wifi.connect(selected) {
        Ok(()) => ProgramResult::ok(format!("connected to {}", selected)),
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
    // Return a special output that the shell recognises as a list request.
    // The shell will parse this and start interactive list mode.
    // Format: "__START_LIST__\nheader\nitem1\nitem2\n..."
    let mut lines = vec!["__START_LIST__".to_string(), "select network:".to_string()];
    lines.extend(networks);
    ProgramResult::ok(lines.join("\n"))
}

fn disconnect(ctx: &mut ExecContext) -> ProgramResult {
    match ctx.wifi.disconnect() {
        Ok(()) => ProgramResult::ok("disconnected".to_string()),
        Err(e) => ProgramResult::err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::{MockWifiDriver, WifiDriver};

    fn make_ctx(wifi: &mut dyn crate::wifi::WifiDriver) -> ExecContext<'_> {
        ExecContext { uptime_secs: 0, wifi }
    }

    #[test]
    fn no_args_shows_usage() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&[], &mut ctx);
        assert!(r.output.contains("usage:"));
    }

    #[test]
    fn unknown_subcommand() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&["foo"], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert!(r.output.contains("unknown subcommand"));
    }

    #[test]
    fn status_when_disconnected() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&["status"], &mut ctx);
        assert_eq!(r.output, "not connected");
    }

    #[test]
    fn status_when_connected() {
        let mut wifi = MockWifiDriver::new();
        wifi.connect("home_wifi").unwrap();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&["status"], &mut ctx);
        assert_eq!(r.output, "connected to home_wifi");
    }

    #[test]
    fn connect_returns_start_list() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&["connect"], &mut ctx);
        assert!(r.output.starts_with("__START_LIST__"));
        assert!(r.output.contains("home_wifi"));
        assert!(r.output.contains("coffee_shop"));
        assert!(r.output.contains("neighbor_5g"));
    }

    #[test]
    fn disconnect_when_connected() {
        let mut wifi = MockWifiDriver::new();
        wifi.connect("home_wifi").unwrap();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&["disconnect"], &mut ctx);
        assert_eq!(r.output, "disconnected");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn disconnect_when_not_connected() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = run(&["disconnect"], &mut ctx);
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.output, "not connected");
    }

    #[test]
    fn on_list_select_connects() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = on_list_select("coffee_shop", &mut ctx);
        assert_eq!(r.output, "connected to coffee_shop");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn on_list_select_invalid_network() {
        let mut wifi = MockWifiDriver::new();
        let mut ctx = make_ctx(&mut wifi);
        let r = on_list_select("doesnt_exist", &mut ctx);
        assert_eq!(r.exit_code, 1);
    }
}
