//! Persistent storage for WiFi credentials (SSID → password).
//!
//! The [`NetworkStore`] trait abstracts over the storage backend.
//! [`MockNetworkStore`] provides an in-memory implementation for tests.
//!
//! Store invariants:
//!   - `save()` overwrites any existing entry for the same SSID
//!   - `delete()` is a no-op if the SSID is not saved
//!   - `list()` returns SSIDs in no guaranteed order

use std::collections::HashMap;

/// Persistence backend for saved WiFi credentials.
pub trait NetworkStore {
    /// Look up the saved password for an SSID. Returns `None` if not saved.
    fn load(&self, ssid: &str) -> Option<String>;
    /// Save (or overwrite) credentials for an SSID.
    fn save(&mut self, ssid: &str, password: &str);
    /// Remove saved credentials. No-op if the SSID is not saved.
    fn delete(&mut self, ssid: &str);
    /// List all saved SSIDs.
    fn list(&self) -> Vec<String>;
}

/// In-memory implementation for tests.
pub struct MockNetworkStore {
    credentials: HashMap<String, String>,
}

impl MockNetworkStore {
    pub fn new() -> Self {
        Self {
            credentials: HashMap::new(),
        }
    }
}

impl NetworkStore for MockNetworkStore {
    fn load(&self, ssid: &str) -> Option<String> {
        self.credentials.get(ssid).cloned()
    }

    fn save(&mut self, ssid: &str, password: &str) {
        self.credentials
            .insert(ssid.to_string(), password.to_string());
    }

    fn delete(&mut self, ssid: &str) {
        self.credentials.remove(ssid);
    }

    fn list(&self) -> Vec<String> {
        self.credentials.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_none_when_empty() {
        let store = MockNetworkStore::new();
        assert_eq!(store.load("home"), None);
    }

    #[test]
    fn save_and_load() {
        let mut store = MockNetworkStore::new();
        store.save("home", "secret123");
        assert_eq!(store.load("home"), Some("secret123".to_string()));
    }

    #[test]
    fn save_overwrites() {
        let mut store = MockNetworkStore::new();
        store.save("home", "old");
        store.save("home", "new");
        assert_eq!(store.load("home"), Some("new".to_string()));
    }

    #[test]
    fn delete_removes_entry() {
        let mut store = MockNetworkStore::new();
        store.save("home", "secret");
        store.delete("home");
        assert_eq!(store.load("home"), None);
    }

    #[test]
    fn delete_nonexistent_is_noop() {
        let mut store = MockNetworkStore::new();
        store.delete("nope"); // should not panic
    }

    #[test]
    fn list_returns_saved_ssids() {
        let mut store = MockNetworkStore::new();
        store.save("alpha", "a");
        store.save("bravo", "b");
        let mut ssids = store.list();
        ssids.sort();
        assert_eq!(ssids, vec!["alpha", "bravo"]);
    }

    #[test]
    fn list_empty_when_no_entries() {
        let store = MockNetworkStore::new();
        assert!(store.list().is_empty());
    }
}
