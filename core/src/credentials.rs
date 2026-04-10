//! Persistent storage for application credentials (currently: Gmail).
//!
//! The [`CredentialStore`] trait abstracts over the storage backend.
//! [`MockCredentialStore`] is the in-memory implementation used by tests;
//! the device builds a real one on top of NVS.
//!
//! Store invariants:
//!   - `set_gmail()` overwrites any existing entry.
//!   - `gmail()` returns `None` until something has been stored.
//!   - The store does not validate addresses or password format — it just
//!     persists whatever the caller hands it.

/// A configured Gmail account: full address + 16-char Google App Password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailCreds {
    pub address: String,
    pub app_password: String,
}

/// Persistence backend for application credentials.
pub trait CredentialStore {
    /// Look up the configured Gmail account, if any.
    fn gmail(&self) -> Option<GmailCreds>;
    /// Save (or overwrite) the configured Gmail account.
    fn set_gmail(&mut self, address: &str, app_password: &str) -> Result<(), String>;
    /// Forget the configured Gmail account. No-op if none is set.
    fn clear_gmail(&mut self) -> Result<(), String>;
}

/// In-memory implementation for tests.
pub struct MockCredentialStore {
    gmail: Option<GmailCreds>,
}

impl MockCredentialStore {
    pub fn new() -> Self {
        Self { gmail: None }
    }
}

impl Default for MockCredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for MockCredentialStore {
    fn gmail(&self) -> Option<GmailCreds> {
        self.gmail.clone()
    }
    fn set_gmail(&mut self, address: &str, app_password: &str) -> Result<(), String> {
        self.gmail = Some(GmailCreds {
            address: address.to_string(),
            app_password: app_password.to_string(),
        });
        Ok(())
    }
    fn clear_gmail(&mut self) -> Result<(), String> {
        self.gmail = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_store_returns_none() {
        let store = MockCredentialStore::new();
        assert!(store.gmail().is_none());
    }

    #[test]
    fn set_and_get() {
        let mut store = MockCredentialStore::new();
        store.set_gmail("me@gmail.com", "abcdefghijklmnop").unwrap();
        let creds = store.gmail().unwrap();
        assert_eq!(creds.address, "me@gmail.com");
        assert_eq!(creds.app_password, "abcdefghijklmnop");
    }

    #[test]
    fn set_overwrites() {
        let mut store = MockCredentialStore::new();
        store.set_gmail("a@gmail.com", "old").unwrap();
        store.set_gmail("b@gmail.com", "new").unwrap();
        let creds = store.gmail().unwrap();
        assert_eq!(creds.address, "b@gmail.com");
        assert_eq!(creds.app_password, "new");
    }

    #[test]
    fn clear_removes_entry() {
        let mut store = MockCredentialStore::new();
        store.set_gmail("me@gmail.com", "x").unwrap();
        store.clear_gmail().unwrap();
        assert!(store.gmail().is_none());
    }

    #[test]
    fn clear_when_empty_is_ok() {
        let mut store = MockCredentialStore::new();
        store.clear_gmail().unwrap();
    }
}
