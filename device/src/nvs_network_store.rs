//! NVS-backed WiFi credential store.
//!
//! All saved networks are kept in a single NVS string value (`KEY_DATA`)
//! holding the output of [`dynatac_core::saved_networks::serialise_entries`].
//! A short version-marker key (`KEY_VERSION`) records that we're on the
//! current format so we know whether to run the one-time migration from
//! the old "one key per SSID" layout.
//!
//! Why a single blob: ESP-IDF NVS keys are limited to 15 ASCII characters.
//! IEEE 802.11 SSIDs are up to 32 characters, so using the SSID directly
//! as a key crashes with `ESP_ERR_NVS_KEY_TOO_LONG` for any longer name.
//! Storing everything in one value sidesteps the limit entirely.
//!
//! NVS values are stored in plaintext. Once ESP32-S3 flash encryption is
//! enabled, NVS encryption protects these credentials with no code changes.

use dynatac_core::saved_networks::{parse_entries, serialise_entries, NetworkStore};

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

/// Single NVS string value holding all SSID/password pairs.
const KEY_DATA: &str = "creds";
/// Marker that the store has been migrated to the new format. Its value
/// is the format version as a string (currently `"1"`).
const KEY_VERSION: &str = "fmt_v";
/// Legacy index key from the original "one NVS key per SSID" layout.
/// Read once during migration, then deleted.
const KEY_LEGACY_INDEX: &str = "_index";

/// Read-buffer size for NVS string values. NVS supports values up to ~4 KB;
/// 2 KB is comfortably enough for tens of saved networks.
const MAX_NVS_STR_LEN: usize = 2048;

pub struct NvsNetworkStore {
    nvs: EspNvs<NvsDefault>,
}

impl NvsNetworkStore {
    pub fn new(partition: EspDefaultNvsPartition) -> Self {
        let nvs = EspNvs::new(partition, "wifi_creds", true).unwrap();
        let mut store = Self { nvs };
        store.migrate_if_needed();
        store
    }

    fn get_str(&self, key: &str) -> Option<String> {
        let mut buf = vec![0u8; MAX_NVS_STR_LEN];
        match self.nvs.get_str(key, &mut buf) {
            Ok(Some(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Read all saved networks. Returns an empty list if nothing is saved.
    fn read_all(&self) -> Vec<(String, String)> {
        match self.get_str(KEY_DATA) {
            Some(s) if !s.is_empty() => parse_entries(&s),
            _ => Vec::new(),
        }
    }

    /// Replace the stored set of networks. Logs and ignores write errors so
    /// an NVS hiccup doesn't panic the device.
    fn write_all(&mut self, entries: &[(String, String)]) {
        let serialised = serialise_entries(entries);
        if let Err(e) = self.nvs.set_str(KEY_DATA, &serialised) {
            log::error!("nvs set {}: {:?}", KEY_DATA, e);
        }
    }

    /// One-time migration from the legacy "one NVS key per SSID" layout.
    /// Recovers any short-keyed entries (SSIDs > 15 chars never made it to
    /// NVS in the old layout — they crashed on save) and writes them to
    /// the new single-blob format. Idempotent: skipped on subsequent boots
    /// once `KEY_VERSION` is set.
    fn migrate_if_needed(&mut self) {
        if self.get_str(KEY_VERSION).is_some() {
            return;
        }
        log::info!("migrating wifi_creds NVS layout to v1");

        let mut recovered: Vec<(String, String)> = Vec::new();
        if let Some(legacy_index) = self.get_str(KEY_LEGACY_INDEX) {
            for ssid in legacy_index.lines() {
                if let Some(pw) = self.get_str(ssid) {
                    recovered.push((ssid.to_string(), pw));
                } else {
                    log::warn!(
                        "wifi_creds migration: legacy index lists SSID {} \
                         but no value is stored for it (skipping)",
                        ssid
                    );
                }
            }
        }

        if !recovered.is_empty() {
            self.write_all(&recovered);
            log::info!("migrated {} saved network(s)", recovered.len());
        }

        // Clean up legacy keys regardless of whether anything was recovered,
        // so the old layout never gets read again.
        let _ = self.nvs.remove(KEY_LEGACY_INDEX);
        for (ssid, _) in &recovered {
            let _ = self.nvs.remove(ssid);
        }

        if let Err(e) = self.nvs.set_str(KEY_VERSION, "1") {
            log::error!("nvs set {}: {:?}", KEY_VERSION, e);
        }
    }
}

impl NetworkStore for NvsNetworkStore {
    fn load(&self, ssid: &str) -> Option<String> {
        self.read_all()
            .into_iter()
            .find(|(s, _)| s == ssid)
            .map(|(_, pw)| pw)
    }

    fn save(&mut self, ssid: &str, password: &str) {
        let mut entries = self.read_all();
        if let Some(existing) = entries.iter_mut().find(|(s, _)| s == ssid) {
            existing.1 = password.to_string();
        } else {
            entries.push((ssid.to_string(), password.to_string()));
        }
        self.write_all(&entries);
    }

    fn delete(&mut self, ssid: &str) {
        let mut entries = self.read_all();
        let before = entries.len();
        entries.retain(|(s, _)| s != ssid);
        if entries.len() != before {
            self.write_all(&entries);
        }
    }

    fn list(&self) -> Vec<String> {
        self.read_all().into_iter().map(|(s, _)| s).collect()
    }
}
