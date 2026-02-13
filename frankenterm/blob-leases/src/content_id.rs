#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use sha2::Digest;

/// Identifies data within the store.
/// This is an (unspecified) hash of the content
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ContentId([u8; 32]);

impl ContentId {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let mut hasher = sha2::Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    pub fn as_hash_bytes(&self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Display for ContentId {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "sha256-")?;
        for byte in &self.0 {
            write!(fmt, "{byte:x}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ContentId {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(fmt, "ContentId({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_bytes_is_deterministic() {
        let a = ContentId::for_bytes(b"hello world");
        let b = ContentId::for_bytes(b"hello world");
        assert_eq!(a, b);
        assert_eq!(a.as_hash_bytes(), b.as_hash_bytes());
    }

    #[test]
    fn different_content_produces_different_ids() {
        let a = ContentId::for_bytes(b"aaa");
        let b = ContentId::for_bytes(b"bbb");
        assert_ne!(a, b);
    }

    #[test]
    fn empty_content_has_valid_id() {
        let id = ContentId::for_bytes(b"");
        assert_ne!(id.as_hash_bytes(), [0u8; 32]);
    }

    #[test]
    fn display_starts_with_sha256_prefix() {
        let id = ContentId::for_bytes(b"test");
        let display = format!("{id}");
        assert!(display.starts_with("sha256-"), "got: {display}");
    }

    #[test]
    fn display_contains_only_hex_after_prefix() {
        let id = ContentId::for_bytes(b"data");
        let display = format!("{id}");
        let hex_part = display.strip_prefix("sha256-").unwrap();
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn debug_wraps_display() {
        let id = ContentId::for_bytes(b"test");
        let debug = format!("{id:?}");
        assert!(debug.starts_with("ContentId(sha256-"));
        assert!(debug.ends_with(')'));
    }

    #[test]
    fn hash_bytes_is_32_bytes() {
        let id = ContentId::for_bytes(b"any content");
        assert_eq!(id.as_hash_bytes().len(), 32);
    }

    #[test]
    fn clone_produces_equal_id() {
        let id = ContentId::for_bytes(b"clone test");
        let cloned = id;
        assert_eq!(id, cloned);
    }

    #[test]
    fn can_be_used_as_hash_key() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        let id = ContentId::for_bytes(b"key");
        set.insert(id);
        assert!(set.contains(&id));
        set.insert(ContentId::for_bytes(b"key"));
        assert_eq!(set.len(), 1);
    }
}
