//! NVS-backed credential store.
//!
//! Stores the configured Gmail account in the `"app_creds"` NVS namespace.
//! Two keys: `"gmail_addr"` and `"gmail_pw"`. Both must be present for
//! `gmail()` to return `Some`.
//!
//! Values are stored in plaintext. Once ESP32-S3 flash encryption is
//! enabled, NVS encryption will protect them transparently — same trust
//! model as the WiFi credential store next door.

use dynatac_core::credentials::{CredentialStore, GmailCreds};

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

const NAMESPACE: &str = "app_creds";
const KEY_ADDR: &str = "gmail_addr";
const KEY_PW: &str = "gmail_pw";

/// Max length of a single NVS string value we read.
const MAX_NVS_STR_LEN: usize = 256;

pub struct NvsCredentialStore {
    nvs: EspNvs<NvsDefault>,
}

impl NvsCredentialStore {
    pub fn new(partition: EspDefaultNvsPartition) -> Self {
        let nvs = EspNvs::new(partition, NAMESPACE, true).unwrap();
        Self { nvs }
    }

    fn get_str(&self, key: &str) -> Option<String> {
        let mut buf = vec![0u8; MAX_NVS_STR_LEN];
        match self.nvs.get_str(key, &mut buf) {
            Ok(Some(s)) => Some(s.to_string()),
            _ => None,
        }
    }
}

impl CredentialStore for NvsCredentialStore {
    fn gmail(&self) -> Option<GmailCreds> {
        let address = self.get_str(KEY_ADDR)?;
        let app_password = self.get_str(KEY_PW)?;
        Some(GmailCreds {
            address,
            app_password,
        })
    }

    fn set_gmail(&mut self, address: &str, app_password: &str) -> Result<(), String> {
        self.nvs
            .set_str(KEY_ADDR, address)
            .map_err(|e| format!("nvs set {}: {:?}", KEY_ADDR, e))?;
        self.nvs
            .set_str(KEY_PW, app_password)
            .map_err(|e| format!("nvs set {}: {:?}", KEY_PW, e))?;
        Ok(())
    }

    fn clear_gmail(&mut self) -> Result<(), String> {
        let _ = self.nvs.remove(KEY_ADDR);
        let _ = self.nvs.remove(KEY_PW);
        Ok(())
    }
}
