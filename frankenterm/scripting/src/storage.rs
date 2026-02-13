//! Per-extension key-value storage.
//!
//! Each extension gets an isolated key-value namespace that persists
//! across sessions via the filesystem. Keys and values are byte strings.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Per-extension key-value store backed by a directory of files.
///
/// Each extension gets its own subdirectory. Keys are mapped to files
/// within that directory (with safe filename encoding).
pub struct ExtensionStorage {
    base_dir: PathBuf,
    /// In-memory cache per extension: extension_id → (key → value)
    cache: Mutex<HashMap<String, HashMap<String, Vec<u8>>>>,
}

impl ExtensionStorage {
    /// Create a storage manager rooted at the given directory.
    pub fn new(base_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&base_dir).with_context(|| {
            format!("failed to create storage directory {}", base_dir.display())
        })?;
        Ok(Self {
            base_dir,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Get a value for an extension.
    pub fn get(&self, extension_id: &str, key: &str) -> Result<Option<Vec<u8>>> {
        validate_key(key)?;

        // Check memory cache first
        if let Ok(cache) = self.cache.lock()
            && let Some(value) = cache.get(extension_id).and_then(|ec| ec.get(key))
        {
            return Ok(Some(value.clone()));
        }

        // Fall through to disk
        let path = self.key_path(extension_id, key);
        if !path.exists() {
            return Ok(None);
        }

        let data =
            std::fs::read(&path).with_context(|| format!("failed to read storage key '{key}'"))?;

        // Populate cache
        if let Ok(mut cache) = self.cache.lock() {
            cache
                .entry(extension_id.to_string())
                .or_default()
                .insert(key.to_string(), data.clone());
        }

        Ok(Some(data))
    }

    /// Set a value for an extension.
    pub fn set(&self, extension_id: &str, key: &str, value: &[u8]) -> Result<()> {
        validate_key(key)?;

        let ext_dir = self.base_dir.join(safe_dirname(extension_id));
        std::fs::create_dir_all(&ext_dir).with_context(|| {
            format!(
                "failed to create extension storage dir {}",
                ext_dir.display()
            )
        })?;

        let path = self.key_path(extension_id, key);
        std::fs::write(&path, value)
            .with_context(|| format!("failed to write storage key '{key}'"))?;

        // Update cache
        if let Ok(mut cache) = self.cache.lock() {
            cache
                .entry(extension_id.to_string())
                .or_default()
                .insert(key.to_string(), value.to_vec());
        }

        Ok(())
    }

    /// Delete a key for an extension.
    pub fn delete(&self, extension_id: &str, key: &str) -> Result<bool> {
        validate_key(key)?;

        let path = self.key_path(extension_id, key);
        let existed = path.exists();

        if existed {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to delete storage key '{key}'"))?;
        }

        // Remove from cache
        if let Ok(mut cache) = self.cache.lock()
            && let Some(ext_cache) = cache.get_mut(extension_id)
        {
            ext_cache.remove(key);
        }

        Ok(existed)
    }

    /// List all keys for an extension.
    pub fn keys(&self, extension_id: &str) -> Result<Vec<String>> {
        let ext_dir = self.base_dir.join(safe_dirname(extension_id));
        if !ext_dir.exists() {
            return Ok(Vec::new());
        }

        let mut keys = Vec::new();
        for entry in std::fs::read_dir(&ext_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if let Some(name) = file_name.to_str()
                && let Some(key) = decode_filename(name)
            {
                keys.push(key);
            }
        }

        keys.sort();
        Ok(keys)
    }

    /// Delete all data for an extension.
    pub fn clear_extension(&self, extension_id: &str) -> Result<()> {
        let ext_dir = self.base_dir.join(safe_dirname(extension_id));
        if ext_dir.exists() {
            std::fs::remove_dir_all(&ext_dir).with_context(|| {
                format!("failed to clear storage for extension '{extension_id}'")
            })?;
        }

        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(extension_id);
        }

        Ok(())
    }

    /// Clear the in-memory cache (does not affect disk).
    pub fn clear_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.clear();
        }
    }

    fn key_path(&self, extension_id: &str, key: &str) -> PathBuf {
        self.base_dir
            .join(safe_dirname(extension_id))
            .join(encode_filename(key))
    }
}

/// Validate that a key is non-empty and within size limits.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("storage key must not be empty");
    }
    if key.len() > 256 {
        bail!("storage key too long (max 256 bytes)");
    }
    Ok(())
}

/// Encode a key as a safe filename (percent-encode unsafe bytes).
fn encode_filename(key: &str) -> String {
    let mut result = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
            result.push(byte as char);
        } else {
            result.push_str(&format!("%{byte:02x}"));
        }
    }
    result.push_str(".dat");
    result
}

/// Decode a filename back to a key, or None if not a valid encoded name.
fn decode_filename(name: &str) -> Option<String> {
    let name = name.strip_suffix(".dat")?;
    let mut bytes = Vec::new();
    let raw = name.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            let hex_str = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex_str, 16).ok()?;
            bytes.push(byte);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).ok()
}

/// Convert an extension ID to a safe directory name.
fn safe_dirname(extension_id: &str) -> String {
    extension_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext-1", "theme", b"dark").unwrap();
        let value = storage.get("ext-1", "theme").unwrap();
        assert_eq!(value.as_deref(), Some(b"dark".as_slice()));
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        let value = storage.get("ext-1", "missing").unwrap();
        assert!(value.is_none());
    }

    #[test]
    fn overwrite_value() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext-1", "key", b"v1").unwrap();
        storage.set("ext-1", "key", b"v2").unwrap();

        let value = storage.get("ext-1", "key").unwrap();
        assert_eq!(value.as_deref(), Some(b"v2".as_slice()));
    }

    #[test]
    fn delete_key() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext-1", "key", b"value").unwrap();
        assert!(storage.delete("ext-1", "key").unwrap());
        assert!(storage.get("ext-1", "key").unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent_returns_false() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        assert!(!storage.delete("ext-1", "missing").unwrap());
    }

    #[test]
    fn list_keys() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext-1", "alpha", b"a").unwrap();
        storage.set("ext-1", "beta", b"b").unwrap();
        storage.set("ext-1", "gamma", b"g").unwrap();

        let keys = storage.keys("ext-1").unwrap();
        assert_eq!(keys, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn list_keys_empty() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        let keys = storage.keys("ext-1").unwrap();
        assert!(keys.is_empty());
    }

    #[test]
    fn extensions_are_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext-1", "key", b"from-ext-1").unwrap();
        storage.set("ext-2", "key", b"from-ext-2").unwrap();

        assert_eq!(
            storage.get("ext-1", "key").unwrap().as_deref(),
            Some(b"from-ext-1".as_slice())
        );
        assert_eq!(
            storage.get("ext-2", "key").unwrap().as_deref(),
            Some(b"from-ext-2".as_slice())
        );
    }

    #[test]
    fn clear_extension() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext-1", "a", b"1").unwrap();
        storage.set("ext-1", "b", b"2").unwrap();
        storage.clear_extension("ext-1").unwrap();

        assert!(storage.keys("ext-1").unwrap().is_empty());
    }

    #[test]
    fn special_chars_in_key() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext", "my.key/path", b"value").unwrap();
        let val = storage.get("ext", "my.key/path").unwrap();
        assert_eq!(val.as_deref(), Some(b"value".as_slice()));

        let keys = storage.keys("ext").unwrap();
        assert_eq!(keys, vec!["my.key/path"]);
    }

    #[test]
    fn empty_key_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        assert!(storage.set("ext", "", b"val").is_err());
        assert!(storage.get("ext", "").is_err());
    }

    #[test]
    fn long_key_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        let long_key = "x".repeat(300);
        assert!(storage.set("ext", &long_key, b"val").is_err());
    }

    #[test]
    fn cache_cleared_separately_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

        storage.set("ext", "key", b"value").unwrap();
        storage.clear_cache();

        // Should still load from disk
        let val = storage.get("ext", "key").unwrap();
        assert_eq!(val.as_deref(), Some(b"value".as_slice()));
    }

    #[test]
    fn encode_decode_filename_roundtrip() {
        let keys = &["simple", "with.dots", "with/slashes", "a=b&c=d", "emoji💡"];
        for key in keys {
            let encoded = encode_filename(key);
            let decoded = decode_filename(&encoded).unwrap();
            assert_eq!(&decoded, key, "roundtrip failed for '{key}'");
        }
    }
}
