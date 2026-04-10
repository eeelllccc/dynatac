//! Persistent storage for WiFi credentials (SSID → password).
//!
//! The [`NetworkStore`] trait abstracts over the storage backend.
//! [`MockNetworkStore`] provides an in-memory implementation for tests.
//! [`serialise_entries`] / [`parse_entries`] are the canonical text format
//! used by the NVS-backed implementation in `device/src/nvs_network_store.rs`,
//! lifted into `core/` so they can be host-tested.
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

/// Serialise a list of `(ssid, password)` pairs into a single string suitable
/// for storage in a single NVS value.
///
/// Format: alternating lines, SSID then password, each terminated by `\n`.
/// Both fields are stored verbatim — newlines in either are unsupported and
/// the parser will treat them as record separators.
///
/// This format is safe for real WiFi credentials: SSIDs and WPA2 passphrases
/// are restricted to printable ASCII, so newlines never legitimately appear.
pub fn serialise_entries(entries: &[(String, String)]) -> String {
    let mut out = String::new();
    for (ssid, password) in entries {
        out.push_str(ssid);
        out.push('\n');
        out.push_str(password);
        out.push('\n');
    }
    out
}

/// Inverse of [`serialise_entries`]. Returns an empty list if the input is
/// empty, malformed (odd number of lines), or unparsable. Never panics.
pub fn parse_entries(s: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() % 2 != 0 {
        return Vec::new();
    }
    lines
        .chunks(2)
        .map(|pair| (pair[0].to_string(), pair[1].to_string()))
        .collect()
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

    // --- serialise_entries / parse_entries ----------------------------------

    #[test]
    fn serialise_empty_is_empty_string() {
        assert_eq!(serialise_entries(&[]), "");
    }

    #[test]
    fn serialise_single_entry() {
        let entries = vec![("home".to_string(), "hunter2".to_string())];
        assert_eq!(serialise_entries(&entries), "home\nhunter2\n");
    }

    #[test]
    fn serialise_multiple_entries() {
        let entries = vec![
            ("home".to_string(), "pw1".to_string()),
            ("cafe".to_string(), "pw2".to_string()),
        ];
        assert_eq!(serialise_entries(&entries), "home\npw1\ncafe\npw2\n");
    }

    #[test]
    fn parse_empty_string_yields_empty_vec() {
        assert!(parse_entries("").is_empty());
    }

    #[test]
    fn parse_round_trips_serialise() {
        let entries = vec![
            ("alpha".to_string(), "secret1".to_string()),
            ("bravo with spaces".to_string(), "p@ss w0rd".to_string()),
            (
                // 32-char SSID — the IEEE 802.11 max — to confirm long names
                // survive serialisation.
                "this_ssid_is_thirty_two_chars_xx".to_string(),
                "63CharacterPassphraseExampleAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
            ),
        ];
        let serialised = serialise_entries(&entries);
        assert_eq!(parse_entries(&serialised), entries);
    }

    #[test]
    fn parse_odd_number_of_lines_returns_empty() {
        // A truncated/corrupt blob should not produce half-entries.
        assert!(parse_entries("only_ssid_no_password").is_empty());
        assert!(parse_entries("a\nb\nc").is_empty());
    }

    #[test]
    fn parse_handles_blank_password() {
        // Open networks have an empty password — must round-trip.
        let entries = vec![("openwifi".to_string(), "".to_string())];
        let s = serialise_entries(&entries);
        assert_eq!(parse_entries(&s), entries);
    }

    #[test]
    fn parse_handles_blank_ssid() {
        // Pathological but possible. The format must still parse.
        let entries = vec![("".to_string(), "pw".to_string())];
        let s = serialise_entries(&entries);
        assert_eq!(parse_entries(&s), entries);
    }
}
