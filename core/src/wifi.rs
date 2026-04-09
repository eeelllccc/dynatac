//! WiFi driver trait and mock implementation.
//!
//! The trait defines the interface that any WiFi hardware driver must
//! implement. `MockWifiDriver` provides a test double with 3 hardcoded
//! networks and in-memory connection state.
//!
//! Driver invariants:
//!   - `scan()` returns the list of visible networks (never empty in mock)
//!   - `connect(name)` fails if the network doesn't exist
//!   - `disconnect()` fails if not currently connected
//!   - `status()` reflects the current connection state

/// Current connection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiStatus {
    Connected(String),
    Disconnected,
}

/// Hardware-agnostic WiFi interface.
pub trait WifiDriver {
    fn scan(&self) -> Vec<String>;
    fn status(&self) -> WifiStatus;
    fn connect(&mut self, network: &str) -> Result<(), String>;
    fn disconnect(&mut self) -> Result<(), String>;
}

/// Test double: 3 hardcoded networks, in-memory connection tracking.
pub struct MockWifiDriver {
    networks: Vec<String>,
    connected: Option<String>,
}

impl MockWifiDriver {
    pub fn new() -> Self {
        Self {
            networks: vec![
                "home_wifi".to_string(),
                "coffee_shop".to_string(),
                "neighbor_5g".to_string(),
            ],
            connected: None,
        }
    }
}

impl WifiDriver for MockWifiDriver {
    fn scan(&self) -> Vec<String> {
        self.networks.clone()
    }

    fn status(&self) -> WifiStatus {
        match &self.connected {
            Some(name) => WifiStatus::Connected(name.clone()),
            None => WifiStatus::Disconnected,
        }
    }

    fn connect(&mut self, network: &str) -> Result<(), String> {
        if !self.networks.iter().any(|n| n == network) {
            return Err(format!("network not found: {}", network));
        }
        self.connected = Some(network.to_string());
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), String> {
        if self.connected.is_none() {
            return Err("not connected".to_string());
        }
        self.connected = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_returns_three_networks() {
        let driver = MockWifiDriver::new();
        let networks = driver.scan();
        assert_eq!(networks.len(), 3);
        assert!(networks.contains(&"home_wifi".to_string()));
        assert!(networks.contains(&"coffee_shop".to_string()));
        assert!(networks.contains(&"neighbor_5g".to_string()));
    }

    #[test]
    fn initially_disconnected() {
        let driver = MockWifiDriver::new();
        assert_eq!(driver.status(), WifiStatus::Disconnected);
    }

    #[test]
    fn connect_to_existing_network() {
        let mut driver = MockWifiDriver::new();
        assert!(driver.connect("home_wifi").is_ok());
        assert_eq!(driver.status(), WifiStatus::Connected("home_wifi".to_string()));
    }

    #[test]
    fn connect_to_nonexistent_network_fails() {
        let mut driver = MockWifiDriver::new();
        let err = driver.connect("doesnt_exist").unwrap_err();
        assert_eq!(err, "network not found: doesnt_exist");
    }

    #[test]
    fn disconnect_when_connected() {
        let mut driver = MockWifiDriver::new();
        driver.connect("coffee_shop").unwrap();
        assert!(driver.disconnect().is_ok());
        assert_eq!(driver.status(), WifiStatus::Disconnected);
    }

    #[test]
    fn disconnect_when_not_connected_fails() {
        let mut driver = MockWifiDriver::new();
        let err = driver.disconnect().unwrap_err();
        assert_eq!(err, "not connected");
    }

    #[test]
    fn connect_switches_network() {
        let mut driver = MockWifiDriver::new();
        driver.connect("home_wifi").unwrap();
        driver.connect("coffee_shop").unwrap();
        assert_eq!(driver.status(), WifiStatus::Connected("coffee_shop".to_string()));
    }
}
