//! Real WiFi driver using ESP-IDF's BlockingWifi.
//!
//! Wraps `BlockingWifi<EspWifi>` and implements the `WifiDriver` trait
//! from `dynatac_core`. WiFi is started on construction and stays active.
//!
//! Driver invariants:
//!   - WiFi is in station mode and started after `new()` returns
//!   - `scan()` triggers a fresh scan each call (blocking)
//!   - `connect()` sets the configuration and blocks until associated
//!   - `disconnect()` fails if not currently connected

use dynatac_core::wifi::{WifiDriver, WifiStatus};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripheral::Peripheral;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AccessPointInfo, AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
};

pub struct EspWifiDriver<'d> {
    wifi: BlockingWifi<EspWifi<'d>>,
    connected_ssid: Option<String>,
}

impl<'d> EspWifiDriver<'d> {
    /// Create and start the WiFi driver in station mode.
    pub fn new(
        modem: impl Peripheral<P = esp_idf_svc::hal::modem::Modem> + 'd,
        sysloop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
    ) -> Self {
        let esp_wifi = EspWifi::new(modem, sysloop.clone(), nvs).unwrap();
        let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop).unwrap();

        wifi.set_configuration(&Configuration::Client(ClientConfiguration::default()))
            .unwrap();
        wifi.start().unwrap();

        Self {
            wifi,
            connected_ssid: None,
        }
    }

    /// Disconnect (if connected) and stop the WiFi radio entirely.
    /// Used by the lockscreen path before entering light sleep so
    /// the radio isn't drawing current while the device is locked.
    /// Returns the first error encountered but does not abort early —
    /// every step is best-effort.
    pub fn shutdown_for_sleep(&mut self) -> Result<(), String> {
        let mut first_err: Option<String> = None;
        if self.connected_ssid.is_some() {
            if let Err(e) = self.wifi.disconnect() {
                first_err.get_or_insert(format!("disconnect error: {:?}", e));
            }
            self.connected_ssid = None;
        }
        if let Err(e) = self.wifi.stop() {
            first_err.get_or_insert(format!("stop error: {:?}", e));
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl WifiDriver for EspWifiDriver<'_> {
    fn scan(&mut self) -> Vec<String> {
        match self.wifi.scan() {
            Ok(aps) => {
                let mut names: Vec<String> = aps
                    .iter()
                    .map(|ap: &AccessPointInfo| ap.ssid.to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                names.dedup();
                names
            }
            Err(e) => {
                log::error!("wifi scan failed: {:?}", e);
                Vec::new()
            }
        }
    }

    fn connect(&mut self, network: &str, password: &str) -> Result<(), String> {
        let auth = if password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2Personal
        };

        let config = Configuration::Client(ClientConfiguration {
            ssid: network.try_into().map_err(|_| "SSID too long".to_string())?,
            password: password
                .try_into()
                .map_err(|_| "password too long".to_string())?,
            auth_method: auth,
            ..Default::default()
        });

        self.wifi
            .set_configuration(&config)
            .map_err(|e| format!("config error: {:?}", e))?;

        self.wifi
            .connect()
            .map_err(|e| format!("connect error: {:?}", e))?;

        self.wifi
            .wait_netif_up()
            .map_err(|e| format!("netif error: {:?}", e))?;

        self.connected_ssid = Some(network.to_string());
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), String> {
        if self.connected_ssid.is_none() {
            return Err("not connected".to_string());
        }
        self.wifi
            .disconnect()
            .map_err(|e| format!("disconnect error: {:?}", e))?;
        self.connected_ssid = None;
        Ok(())
    }

    fn status(&self) -> WifiStatus {
        match &self.connected_ssid {
            Some(ssid) => WifiStatus::Connected(ssid.clone()),
            None => WifiStatus::Disconnected,
        }
    }
}
