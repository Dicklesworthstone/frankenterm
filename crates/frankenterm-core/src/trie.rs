//! Compact prefix trie for efficient string operations.
//!
//! Provides fast prefix matching, auto-completion, and longest common prefix
//! queries. All keys are byte strings (`Vec<u8>`), but convenience methods
//! accept `&str` for UTF-8 strings.
//!
//! # Use Cases
//!
//! - Command completion and auto-suggest in terminal UI
//! - Path prefix matching for pane routing
//! - Shared-prefix deduplication in session names
//! - Fast longest-common-prefix queries across key sets
//!
//! # Complexity
//!
//! | Operation            | Time     |
//! |---------------------|----------|
//! | `insert`            | O(|key|) |
//! | `contains`          | O(|key|) |
//! | `starts_with`       | O(|prefix|) |
//! | `remove`            | O(|key|) |
//! | `keys_with_prefix`  | O(|prefix| + results) |
//! | `longest_common_prefix` | O(|key|) |
//!
//! Bead: ft-283h4.29

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Configuration for a Trie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieConfig {
    /// Expected maximum number of keys (for documentation; doesn't pre-allocate).
    pub expected_keys: usize,
}

impl Default for TrieConfig {
    fn default() -> Self {
        Self { expected_keys: 256 }
    }
}

/// Statistics about a Trie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrieStats {
    /// Number of keys stored.
    pub key_count: usize,
    /// Total number of trie nodes.
    pub node_count: usize,
    /// Number of insert operations performed.
    pub insert_count: u64,
    /// Number of lookup operations performed.
    pub lookup_count: u64,
    /// Approximate memory usage in bytes.
    pub memory_bytes: usize,
}

/// A node in the trie.
#[derive(Debug, Clone)]
struct TrieNode {
    /// Children indexed by byte value.
    children: HashMap<u8, TrieNode>,
    /// Whether this node marks the end of a key.
    is_terminal: bool,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            children: HashMap::new(),
            is_terminal: false,
        }
    }

    /// Count total nodes in subtree (including self).
    fn node_count(&self) -> usize {
        1 + self.children.values().map(TrieNode::node_count).sum::<usize>()
    }

    /// Approximate memory of this node and subtree.
    fn memory_bytes(&self) -> usize {
        let self_size = std::mem::size_of::<Self>()
            + self.children.capacity() * (std::mem::size_of::<u8>() + std::mem::size_of::<TrieNode>());
        self_size + self.children.values().map(TrieNode::memory_bytes).sum::<usize>()
    }

    /// Collect all keys in subtree.
    fn collect_keys(&self, prefix: &mut Vec<u8>, results: &mut Vec<Vec<u8>>) {
        if self.is_terminal {
            results.push(prefix.clone());
        }
        // Iterate in sorted order for deterministic output
        let mut keys: Vec<u8> = self.children.keys().copied().collect();
        keys.sort_unstable();
        for byte in keys {
            if let Some(child) = self.children.get(&byte) {
                prefix.push(byte);
                child.collect_keys(prefix, results);
                prefix.pop();
            }
        }
    }

    /// Check if subtree has any terminal nodes.
    fn has_any_key(&self) -> bool {
        if self.is_terminal {
            return true;
        }
        self.children.values().any(TrieNode::has_any_key)
    }
}

/// Compact prefix trie for byte-string keys.
///
/// # Example
///
/// ```
/// use frankenterm_core::trie::Trie;
///
/// let mut t = Trie::new();
/// t.insert("hello");
/// t.insert("help");
/// t.insert("world");
///
/// assert!(t.contains("hello"));
/// assert!(t.starts_with("hel"));
/// assert_eq!(t.keys_with_prefix("hel").len(), 2);
/// assert_eq!(t.longest_common_prefix("helping"), "help");
/// ```
#[derive(Debug, Clone)]
pub struct Trie {
    root: TrieNode,
    /// Number of distinct keys.
    key_count: usize,
    /// Operation counters.
    insert_ops: u64,
    lookup_ops: u64,
}

impl Trie {
    /// Create an empty trie.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: TrieNode::new(),
            key_count: 0,
            insert_ops: 0,
            lookup_ops: 0,
        }
    }

    /// Number of distinct keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.key_count
    }

    /// Whether the trie is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.key_count == 0
    }

    /// Insert a key (as bytes). Returns `true` if the key was new.
    pub fn insert_bytes(&mut self, key: &[u8]) -> bool {
        self.insert_ops += 1;
        let mut node = &mut self.root;
        for &byte in key {
            node = node.children.entry(byte).or_insert_with(TrieNode::new);
        }
        if node.is_terminal {
            false
        } else {
            node.is_terminal = true;
            self.key_count += 1;
            true
        }
    }

    /// Insert a string key. Returns `true` if the key was new.
    pub fn insert(&mut self, key: &str) -> bool {
        self.insert_bytes(key.as_bytes())
    }

    /// Check if a key exists (exact match).
    pub fn contains_bytes(&mut self, key: &[u8]) -> bool {
        self.lookup_ops += 1;
        let mut node = &self.root;
        for &byte in key {
            match node.children.get(&byte) {
                Some(child) => node = child,
                None => return false,
            }
        }
        node.is_terminal
    }

    /// Check if a string key exists (exact match).
    pub fn contains(&mut self, key: &str) -> bool {
        self.contains_bytes(key.as_bytes())
    }

    /// Check if any key starts with the given prefix.
    pub fn starts_with_bytes(&self, prefix: &[u8]) -> bool {
        let mut node = &self.root;
        for &byte in prefix {
            match node.children.get(&byte) {
                Some(child) => node = child,
                None => return false,
            }
        }
        node.has_any_key()
    }

    /// Check if any key starts with the given string prefix.
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.starts_with_bytes(prefix.as_bytes())
    }

    /// Collect all keys that start with the given prefix.
    pub fn keys_with_prefix_bytes(&self, prefix: &[u8]) -> Vec<Vec<u8>> {
        let mut node = &self.root;
        for &byte in prefix {
            match node.children.get(&byte) {
                Some(child) => node = child,
                None => return Vec::new(),
            }
        }
        let mut results = Vec::new();
        let mut current_prefix = prefix.to_vec();
        node.collect_keys(&mut current_prefix, &mut results);
        results
    }

    /// Collect all keys that start with the given string prefix.
    pub fn keys_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.keys_with_prefix_bytes(prefix.as_bytes())
            .into_iter()
            .filter_map(|bytes| String::from_utf8(bytes).ok())
            .collect()
    }

    /// Find the longest prefix of `key` that exists in the trie as a complete key.
    ///
    /// Returns the longest matching prefix as a string. If no prefix matches,
    /// returns an empty string.
    pub fn longest_common_prefix(&mut self, key: &str) -> String {
        self.lookup_ops += 1;
        let mut node = &self.root;
        let mut longest = 0;
        for (i, &byte) in key.as_bytes().iter().enumerate() {
            match node.children.get(&byte) {
                Some(child) => {
                    node = child;
                    if node.is_terminal {
                        longest = i + 1;
                    }
                }
                None => break,
            }
        }
        key[..longest].to_string()
    }

    /// Find the longest common prefix shared by the given key with any path
    /// in the trie (not necessarily a complete key).
    pub fn longest_shared_prefix(&self, key: &str) -> String {
        let mut node = &self.root;
        let mut depth = 0;
        for &byte in key.as_bytes() {
            match node.children.get(&byte) {
                Some(child) => {
                    node = child;
                    depth += 1;
                }
                None => break,
            }
        }
        key[..depth].to_string()
    }

    /// Remove a key. Returns `true` if the key existed.
    pub fn remove(&mut self, key: &str) -> bool {
        self.remove_bytes(key.as_bytes())
    }

    /// Remove a byte-string key. Returns `true` if the key existed.
    pub fn remove_bytes(&mut self, key: &[u8]) -> bool {
        if Self::remove_recursive(&mut self.root, key, 0) {
            self.key_count -= 1;
            true
        } else {
            false
        }
    }

    fn remove_recursive(node: &mut TrieNode, key: &[u8], depth: usize) -> bool {
        if depth == key.len() {
            if node.is_terminal {
                node.is_terminal = false;
                return true;
            }
            return false;
        }

        let byte = key[depth];
        let should_remove_child = {
            if let Some(child) = node.children.get_mut(&byte) {
                if Self::remove_recursive(child, key, depth + 1) {
                    // Check if child is now empty and can be pruned
                    !child.is_terminal && child.children.is_empty()
                } else {
                    return false;
                }
            } else {
                return false;
            }
        };

        if should_remove_child {
            node.children.remove(&byte);
        }
        true
    }

    /// Get all keys in sorted order.
    pub fn all_keys(&self) -> Vec<String> {
        let mut results = Vec::new();
        let mut prefix = Vec::new();
        self.root.collect_keys(&mut prefix, &mut results);
        results
            .into_iter()
            .filter_map(|bytes| String::from_utf8(bytes).ok())
            .collect()
    }

    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> TrieStats {
        TrieStats {
            key_count: self.key_count,
            node_count: self.root.node_count(),
            insert_count: self.insert_ops,
            lookup_count: self.lookup_ops,
            memory_bytes: self.memory_bytes(),
        }
    }

    /// Approximate memory usage in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.root.memory_bytes()
    }

    /// Reset the trie to empty.
    pub fn clear(&mut self) {
        self.root = TrieNode::new();
        self.key_count = 0;
    }
}

impl Default for Trie {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_trie() {
        let t = Trie::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn test_insert_and_contains() {
        let mut t = Trie::new();
        assert!(t.insert("hello"));
        assert!(t.contains("hello"));
        assert!(!t.contains("hell"));
        assert!(!t.contains("helloo"));
    }

    #[test]
    fn test_insert_duplicate() {
        let mut t = Trie::new();
        assert!(t.insert("hello"));
        assert!(!t.insert("hello")); // duplicate
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn test_starts_with() {
        let mut t = Trie::new();
        t.insert("hello");
        t.insert("help");
        assert!(t.starts_with("hel"));
        assert!(t.starts_with("hello"));
        assert!(!t.starts_with("hex"));
        assert!(!t.starts_with("world"));
    }

    #[test]
    fn test_keys_with_prefix() {
        let mut t = Trie::new();
        t.insert("hello");
        t.insert("help");
        t.insert("world");

        let mut keys = t.keys_with_prefix("hel");
        keys.sort();
        assert_eq!(keys, vec!["hello", "help"]);

        let keys = t.keys_with_prefix("world");
        assert_eq!(keys, vec!["world"]);

        let keys = t.keys_with_prefix("xyz");
        assert!(keys.is_empty());
    }

    #[test]
    fn test_longest_common_prefix() {
        let mut t = Trie::new();
        t.insert("app");
        t.insert("apple");
        t.insert("application");

        assert_eq!(t.longest_common_prefix("applet"), "apple");
        assert_eq!(t.longest_common_prefix("app"), "app");
        assert_eq!(t.longest_common_prefix("ap"), "");
        assert_eq!(t.longest_common_prefix("application_server"), "application");
    }

    #[test]
    fn test_longest_shared_prefix() {
        let mut t = Trie::new();
        t.insert("hello");
        t.insert("help");

        assert_eq!(t.longest_shared_prefix("helping"), "help");
        assert_eq!(t.longest_shared_prefix("hex"), "he");
        assert_eq!(t.longest_shared_prefix("world"), "");
    }

    #[test]
    fn test_remove() {
        let mut t = Trie::new();
        t.insert("hello");
        t.insert("help");
        assert_eq!(t.len(), 2);

        assert!(t.remove("hello"));
        assert_eq!(t.len(), 1);
        assert!(!t.contains("hello"));
        assert!(t.contains("help"));
    }

    #[test]
    fn test_remove_nonexistent() {
        let mut t = Trie::new();
        t.insert("hello");
        assert!(!t.remove("world"));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn test_remove_prefix_key() {
        let mut t = Trie::new();
        t.insert("he");
        t.insert("hello");

        assert!(t.remove("he"));
        assert!(!t.contains("he"));
        assert!(t.contains("hello"));
        assert!(t.starts_with("he"));
    }

    #[test]
    fn test_remove_longer_key() {
        let mut t = Trie::new();
        t.insert("he");
        t.insert("hello");

        assert!(t.remove("hello"));
        assert!(!t.contains("hello"));
        assert!(t.contains("he"));
    }

    #[test]
    fn test_all_keys_sorted() {
        let mut t = Trie::new();
        t.insert("banana");
        t.insert("apple");
        t.insert("cherry");

        assert_eq!(t.all_keys(), vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn test_empty_string_key() {
        let mut t = Trie::new();
        assert!(t.insert(""));
        assert!(t.contains(""));
        assert_eq!(t.len(), 1);
        assert!(t.remove(""));
        assert!(!t.contains(""));
    }

    #[test]
    fn test_single_char_keys() {
        let mut t = Trie::new();
        t.insert("a");
        t.insert("b");
        t.insert("c");
        assert_eq!(t.len(), 3);
        assert_eq!(t.all_keys(), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_clear() {
        let mut t = Trie::new();
        t.insert("hello");
        t.insert("world");
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert!(!t.contains("hello"));
    }

    #[test]
    fn test_stats() {
        let mut t = Trie::new();
        t.insert("abc");
        t.insert("abd");
        t.contains("abc");

        let stats = t.stats();
        assert_eq!(stats.key_count, 2);
        assert_eq!(stats.insert_count, 2);
        assert_eq!(stats.lookup_count, 1);
        // root -> a -> b -> c, d = 5 nodes
        assert_eq!(stats.node_count, 5);
    }

    #[test]
    fn test_config_serde() {
        let config = TrieConfig { expected_keys: 100 };
        let json = serde_json::to_string(&config).unwrap();
        let back: TrieConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn test_stats_serde() {
        let mut t = Trie::new();
        t.insert("test");
        let stats = t.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: TrieStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn test_clone_independence() {
        let mut t = Trie::new();
        t.insert("hello");
        let mut clone = t.clone();
        clone.insert("world");
        assert_eq!(t.len(), 1);
        assert_eq!(clone.len(), 2);
    }

    #[test]
    fn test_overlapping_prefixes() {
        let mut t = Trie::new();
        t.insert("a");
        t.insert("ab");
        t.insert("abc");
        t.insert("abcd");

        assert_eq!(t.len(), 4);
        assert!(t.contains("a"));
        assert!(t.contains("ab"));
        assert!(t.contains("abc"));
        assert!(t.contains("abcd"));

        let keys = t.keys_with_prefix("ab");
        assert_eq!(keys.len(), 3); // ab, abc, abcd
    }

    #[test]
    fn test_bytes_api() {
        let mut t = Trie::new();
        assert!(t.insert_bytes(b"hello"));
        assert!(t.contains_bytes(b"hello"));
        assert!(!t.contains_bytes(b"hell"));
    }

    #[test]
    fn test_memory_bytes_grows() {
        let t1 = Trie::new();
        let mut t2 = Trie::new();
        for i in 0..100 {
            t2.insert(&format!("key_{i}"));
        }
        assert!(t2.memory_bytes() > t1.memory_bytes());
    }

    #[test]
    fn test_keys_with_prefix_empty_prefix() {
        let mut t = Trie::new();
        t.insert("hello");
        t.insert("world");

        let mut keys = t.keys_with_prefix("");
        keys.sort();
        assert_eq!(keys, vec!["hello", "world"]);
    }
}
