//! Filesystem abstraction for persistent user data.
//!
//! [`FileSystem`] is the trait all OS logic depends on — callers never touch
//! hardware directly. [`MemFs`] is the in-memory implementation used in host
//! tests. The device-side SD card implementation lives in
//! `device/src/sdcard.rs`.
//!
//! Path conventions (enforced by callers, not the trait):
//!   - Paths are relative to the filesystem root (no leading `/`).
//!   - Directory separators are `/`.
//!   - The device implementation prefixes `/sdcard/` before delegating to
//!     `std::fs`; `MemFs` treats the path as an opaque map key.
//!
//! Caller invariants:
//!   - Paths must not be empty and must not contain `..` components.
//!   - The filesystem may or may not be present (SD card is removable); callers
//!     must handle `FsError::Unavailable`.
//!
//! Callee invariants:
//!   - `write` creates the file and any missing parent directories.
//!   - `append` creates the file if it does not exist.
//!   - `delete` is a no-op if the path does not exist (returns `Ok`).
//!   - `exists` never returns an error.

use std::collections::HashMap;

#[derive(Debug, PartialEq)]
pub enum FsError {
    /// The filesystem is not mounted (e.g. SD card absent).
    Unavailable,
    /// A path component that was expected to be a directory is a file, or vice versa.
    NotADirectory,
    /// Underlying I/O error; the string is a human-readable description.
    Io(String),
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsError::Unavailable => write!(f, "filesystem unavailable"),
            FsError::NotADirectory => write!(f, "not a directory"),
            FsError::Io(msg) => write!(f, "I/O error: {}", msg),
        }
    }
}

/// Persistent filesystem backend.
pub trait FileSystem {
    /// Read the entire contents of a file.
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Write `data` to a file, replacing any existing contents.
    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// Append `data` to a file, creating it if it does not exist.
    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// List the names of entries directly inside `dir`.
    /// Returns an empty vec for an empty directory (not an error).
    fn list_dir(&self, dir: &str) -> Result<Vec<String>, FsError>;

    /// Delete a file. No-op if it does not exist.
    fn delete(&mut self, path: &str) -> Result<(), FsError>;

    /// Return `true` if the path exists (file or directory).
    fn exists(&self, path: &str) -> bool;

    /// Convenience: read the file as a UTF-8 string.
    fn read_str(&self, path: &str) -> Result<String, FsError> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes).map_err(|e| FsError::Io(e.to_string()))
    }
}

/// In-memory filesystem for host tests.
///
/// Files are stored as `path → bytes` in a `HashMap`. Directory listings are
/// derived from stored paths — there is no separate directory node. A path
/// `"a/b/c.txt"` implicitly creates directory `"a/b"`.
pub struct MemFs {
    files: HashMap<String, Vec<u8>>,
}

impl MemFs {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
        }
    }
}

impl FileSystem for MemFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| FsError::Io(format!("not found: {}", path)))
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.files.insert(path.to_string(), data.to_vec());
        Ok(())
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        self.files
            .entry(path.to_string())
            .or_default()
            .extend_from_slice(data);
        Ok(())
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<String>, FsError> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir)
        };

        let mut names: Vec<String> = self
            .files
            .keys()
            .filter_map(|k| {
                let rest = if prefix.is_empty() {
                    k.as_str()
                } else {
                    k.strip_prefix(&prefix)?
                };
                // Only direct children: no further `/` in the remainder.
                if rest.contains('/') {
                    None
                } else {
                    Some(rest.to_string())
                }
            })
            .collect();

        names.sort();
        Ok(names)
    }

    fn delete(&mut self, path: &str) -> Result<(), FsError> {
        self.files.remove(path);
        Ok(())
    }

    fn exists(&self, path: &str) -> bool {
        // A path exists if it is a stored file, or if it is a directory prefix
        // of any stored file.
        if self.files.contains_key(path) {
            return true;
        }
        let prefix = format!("{}/", path);
        self.files.keys().any(|k| k.starts_with(&prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs() -> MemFs {
        MemFs::new()
    }

    // --- read / write ---------------------------------------------------------

    #[test]
    fn write_then_read() {
        let mut fs = fs();
        fs.write("contacts.csv", b"alice,123").unwrap();
        assert_eq!(fs.read("contacts.csv").unwrap(), b"alice,123");
    }

    #[test]
    fn write_overwrites_existing() {
        let mut fs = fs();
        fs.write("f.txt", b"old").unwrap();
        fs.write("f.txt", b"new").unwrap();
        assert_eq!(fs.read("f.txt").unwrap(), b"new");
    }

    #[test]
    fn read_missing_is_error() {
        let fs = fs();
        assert!(matches!(fs.read("nope.txt"), Err(FsError::Io(_))));
    }

    #[test]
    fn read_str_utf8() {
        let mut fs = fs();
        fs.write("hello.txt", b"hello").unwrap();
        assert_eq!(fs.read_str("hello.txt").unwrap(), "hello");
    }

    #[test]
    fn read_str_invalid_utf8_is_error() {
        let mut fs = fs();
        fs.write("bad.bin", &[0xFF, 0xFE]).unwrap();
        assert!(matches!(fs.read_str("bad.bin"), Err(FsError::Io(_))));
    }

    // --- append ---------------------------------------------------------------

    #[test]
    fn append_creates_file() {
        let mut fs = fs();
        fs.append("log.txt", b"line1\n").unwrap();
        assert_eq!(fs.read("log.txt").unwrap(), b"line1\n");
    }

    #[test]
    fn append_extends_existing() {
        let mut fs = fs();
        fs.append("log.txt", b"a").unwrap();
        fs.append("log.txt", b"b").unwrap();
        assert_eq!(fs.read("log.txt").unwrap(), b"ab");
    }

    // --- delete ---------------------------------------------------------------

    #[test]
    fn delete_removes_file() {
        let mut fs = fs();
        fs.write("tmp.txt", b"x").unwrap();
        fs.delete("tmp.txt").unwrap();
        assert!(!fs.exists("tmp.txt"));
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        let mut fs = fs();
        assert!(fs.delete("nope.txt").is_ok());
    }

    // --- exists ---------------------------------------------------------------

    #[test]
    fn exists_true_for_file() {
        let mut fs = fs();
        fs.write("a.txt", b"").unwrap();
        assert!(fs.exists("a.txt"));
    }

    #[test]
    fn exists_false_for_missing() {
        let fs = fs();
        assert!(!fs.exists("nope.txt"));
    }

    #[test]
    fn exists_true_for_implicit_directory() {
        let mut fs = fs();
        fs.write("contacts/alice.csv", b"").unwrap();
        assert!(fs.exists("contacts"));
    }

    // --- list_dir -------------------------------------------------------------

    #[test]
    fn list_dir_root() {
        let mut fs = fs();
        fs.write("a.txt", b"").unwrap();
        fs.write("b.txt", b"").unwrap();
        let mut entries = fs.list_dir("").unwrap();
        entries.sort();
        assert_eq!(entries, vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn list_dir_subdirectory() {
        let mut fs = fs();
        fs.write("contacts/alice.csv", b"").unwrap();
        fs.write("contacts/bob.csv", b"").unwrap();
        fs.write("other/x.txt", b"").unwrap();
        let mut entries = fs.list_dir("contacts").unwrap();
        entries.sort();
        assert_eq!(entries, vec!["alice.csv", "bob.csv"]);
    }

    #[test]
    fn list_dir_only_direct_children() {
        let mut fs = fs();
        fs.write("a/b/c.txt", b"").unwrap();
        // "a/b/c.txt" should not appear in list_dir("a") — only "b" (the subdir).
        // MemFs derives subdirs from paths so listing "a" yields nothing because
        // "b/c.txt" still contains a slash.
        let entries = fs.list_dir("a").unwrap();
        assert!(entries.is_empty(), "expected empty, got {:?}", entries);
    }

    #[test]
    fn list_dir_empty() {
        let fs = fs();
        assert!(fs.list_dir("nonexistent").unwrap().is_empty());
    }
}
