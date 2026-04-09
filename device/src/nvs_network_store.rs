//! NVS-backed WiFi credential store.
//!
//! Stores SSID/password pairs in the `"wifi_creds"` NVS namespace.
//! Each SSID is a key whose value is the password. A special `"_index"`
//! key holds a newline-delimited list of saved SSIDs (since NVS does not
//! support key enumeration).
//!
//! NVS values are stored in plaintext. Once ESP32-S3 flash encryption is
//! enabled, NVS encryption protects these credentials with no code changes.

use dynatac_core::saved_networks::NetworkStore;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

/// Max length of a single NVS string value. ESP-IDF NVS supports up to
/// ~4000 bytes per blob, but we cap the index at a reasonable size.
const MAX_NVS_STR_LEN: usize = 2048;

pub struct NvsNetworkStore {
    nvs: EspNvs<NvsDefault>,
}

impl NvsNetworkStore {
    pub fn new(partition: EspDefaultNvsPartition) -> Self {
        let nvs = EspNvs::new(partition, "wifi_creds", true).unwrap();
        Self { nvs }
    }

    /// Read a string value from NVS, returning None if the key doesn't exist.
    fn get_str(&self, key: &str) -> Option<String> {
        let mut buf = vec![0u8; MAX_NVS_STR_LEN];
        match self.nvs.get_str(key, &mut buf) {
            Ok(Some(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Read the index of saved SSIDs.
    fn read_index(&self) -> Vec<String> {
        match self.get_str("_index") {
            Some(s) if !s.is_empty() => s.lines().map(|l| l.to_string()).collect(),
            _ => Vec::new(),
        }
    }

    /// Write the index of saved SSIDs.
    fn write_index(&mut self, ssids: &[String]) {
        let joined = ssids.join("\n");
        self.nvs.set_str("_index", &joined).unwrap();
    }
}

impl NetworkStore for NvsNetworkStore {
    fn load(&self, ssid: &str) -> Option<String> {
        self.get_str(ssid)
    }

    fn save(&mut self, ssid: &str, password: &str) {
        self.nvs.set_str(ssid, password).unwrap();
        let mut index = self.read_index();
        if !index.iter().any(|s| s == ssid) {
            index.push(ssid.to_string());
            self.write_index(&index);
        }
    }

    fn delete(&mut self, ssid: &str) {
        let _ = self.nvs.remove(ssid);
        let index: Vec<String> = self.read_index().into_iter().filter(|s| s != ssid).collect();
        self.write_index(&index);
    }

    fn list(&self) -> Vec<String> {
        self.read_index()
    }
}
