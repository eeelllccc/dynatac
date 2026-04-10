//! Connectivity policy: which IP transport to use for outgoing network I/O.
//!
//! The rule is "prefer WiFi, fall back to cellular data". Programs that
//! make network calls (curl, email, anything else that wants an IP
//! stack) call [`ensure_connectivity`] *before* they try to use the
//! HTTP/SMTP client. It returns an [`ActiveTransport`] indicating which
//! interface is now live.
//!
//! This module is deliberately free of HTTP-client dispatch logic. Once
//! `ensure_connectivity` returns, lwIP has a default route via *some*
//! netif and the existing `EspHttpClient` / `EspSmtpStream` just work
//! over it — we don't choose the transport at the application layer.
//! That's exactly the "abstract at the IP level, not the HTTP level"
//! design decision from our planning discussion, translated into code.
//!
//! The policy is on-demand: cellular data is only brought up when WiFi
//! is disconnected AND a network operation is being attempted. This
//! keeps the radio off by default, which matters for battery life.

use crate::modem::Modem;
use crate::wifi::{WifiDriver, WifiStatus};

/// Hardcoded APN used for cellular data bring-up. EE's PAYG and
/// most contract consumer plans use "everywhere". When we support
/// multiple carriers / configuration this moves into NVS.
pub const APN: &str = "everywhere";

/// Which interface is currently serving outgoing IP traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTransport {
    Wifi,
    Cellular,
}

/// Error surfaced to the caller when no transport can be brought up.
/// Carries a human-readable string so the shell program can print it
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectivityError(pub String);

impl ConnectivityError {
    pub fn display(&self) -> &str {
        &self.0
    }
}

/// Ensure there's a usable IP transport and return which one is active.
///
/// Policy:
///   1. If WiFi is connected, return [`ActiveTransport::Wifi`] immediately
///      (no modem interaction).
///   2. Otherwise, power on the modem if needed and bring up cellular
///      data if needed. Return [`ActiveTransport::Cellular`].
///   3. If the cellular fallback fails at any step, return a
///      [`ConnectivityError`] with a diagnostic message.
///
/// This is synchronous and can block for several seconds while the
/// modem powers on and PPP negotiates. Callers should expect a
/// noticeable delay on the first use after WiFi drops.
pub fn ensure_connectivity(
    wifi: &mut dyn WifiDriver,
    modem: &mut dyn Modem,
    apn: &str,
) -> Result<ActiveTransport, ConnectivityError> {
    if matches!(wifi.status(), WifiStatus::Connected(_)) {
        return Ok(ActiveTransport::Wifi);
    }

    // WiFi isn't available. Fall back to cellular.
    if !modem.is_powered() {
        modem
            .power_on()
            .map_err(|e| ConnectivityError(format!("modem power on: {}", e.display())))?;
    }

    if !modem.is_data_active() {
        modem
            .enable_data(apn)
            .map_err(|e| ConnectivityError(format!("cellular bring-up: {}", e.display())))?;
    }

    Ok(ActiveTransport::Cellular)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modem::{MockModem, ModemError};
    use crate::wifi::{MockWifiDriver, WifiDriver};

    #[test]
    fn uses_wifi_when_connected() {
        let mut wifi = MockWifiDriver::new();
        wifi.connect("home_wifi", "").unwrap();
        let mut modem = MockModem::new();
        let result = ensure_connectivity(&mut wifi, &mut modem, APN).unwrap();
        assert_eq!(result, ActiveTransport::Wifi);
        // Modem must NOT have been touched.
        assert!(!modem.is_powered());
        assert!(!modem.is_data_active());
    }

    #[test]
    fn falls_back_to_cellular_when_wifi_disconnected() {
        let mut wifi = MockWifiDriver::new();
        let mut modem = MockModem::new();
        let result = ensure_connectivity(&mut wifi, &mut modem, APN).unwrap();
        assert_eq!(result, ActiveTransport::Cellular);
        assert!(modem.is_powered());
        assert!(modem.is_data_active());
        assert_eq!(modem.last_apn.as_deref(), Some(APN));
    }

    #[test]
    fn reuses_existing_data_session_without_re_dialling() {
        let mut wifi = MockWifiDriver::new();
        let mut modem = MockModem::new();
        modem.power_on().unwrap();
        modem.enable_data(APN).unwrap();

        let result = ensure_connectivity(&mut wifi, &mut modem, APN).unwrap();
        assert_eq!(result, ActiveTransport::Cellular);
        // Only ever one APN stored — we didn't redial.
        assert_eq!(modem.last_apn.as_deref(), Some(APN));
    }

    #[test]
    fn powers_on_modem_if_needed() {
        let mut wifi = MockWifiDriver::new();
        let mut modem = MockModem::new();
        assert!(!modem.is_powered());
        ensure_connectivity(&mut wifi, &mut modem, APN).unwrap();
        assert!(modem.is_powered());
    }

    #[test]
    fn bubbles_up_cellular_enable_failure() {
        let mut wifi = MockWifiDriver::new();
        let mut modem = MockModem::new();
        modem.enable_data_error = Some(ModemError::Timeout);
        let err = ensure_connectivity(&mut wifi, &mut modem, APN).unwrap_err();
        assert!(err.display().contains("cellular bring-up"));
        assert!(err.display().contains("timeout"));
    }

    #[test]
    fn custom_apn_is_passed_through() {
        let mut wifi = MockWifiDriver::new();
        let mut modem = MockModem::new();
        ensure_connectivity(&mut wifi, &mut modem, "custom-apn").unwrap();
        assert_eq!(modem.last_apn.as_deref(), Some("custom-apn"));
    }
}
