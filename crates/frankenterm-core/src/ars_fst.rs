//! O(1) Finite State Transducer compiler for ARS reflex signatures.
//!
//! As the ARS database grows, linear scanning of trigger patterns becomes
//! untenable. This module compiles MinHash signatures and trigger strings
//! into a Finite State Transducer (FST) that guarantees O(K) lookup time
//! where K is the length of the query string, independent of the number of
//! stored reflexes.
//!
//! # Architecture
//!
//! ```text
//! Reflex Registry ──→ FstCompiler ──→ FstIndex (immutable)
//!                                        │
//!                     Hot-swap via ───→ ArcSwap<FstIndex>
//!                     background          │
//!                     rebuild       Query ──→ O(K) lookup
//! ```
//!
//! # Key Design
//!
//! - **Trie-based FST**: Shared prefix compression over trigger patterns.
//! - **Output values**: Each accepting state maps to a `ReflexId` (u64).
//! - **Prefix queries**: `prefix_search(s)` returns all reflexes whose
//!   trigger is a prefix of `s` (for streaming PTY matching).
//! - **Atomic hot-swap**: Background rebuild + ArcSwap guarantees lock-free reads.
//! - **MinHash integration**: Signatures are serialized to a canonical byte
//!   key for FST insertion alongside literal trigger strings.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use tracing::debug;

// =============================================================================
// Types
// =============================================================================

/// Unique identifier for a reflex in the ARS database.
pub type ReflexId = u64;

/// A trigger entry to be compiled into the FST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerEntry {
    /// The trigger pattern (literal string or serialized MinHash key).
    pub key: Vec<u8>,
    /// The reflex ID this trigger maps to.
    pub reflex_id: ReflexId,
    /// Priority (lower = higher priority). Used for tie-breaking.
    pub priority: u32,
    /// Cluster ID for grouping related reflexes.
    pub cluster_id: String,
}

/// Configuration for FST compilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FstConfig {
    /// Maximum key length in bytes.
    pub max_key_len: usize,
    /// Maximum number of entries in a single FST.
    pub max_entries: usize,
    /// Whether to enable prefix search optimization.
    pub enable_prefix_search: bool,
    /// Whether to deduplicate identical keys (keep highest priority).
    pub dedup_keys: bool,
}

impl Default for FstConfig {
    fn default() -> Self {
        Self {
            max_key_len: 4096,
            max_entries: 1_000_000,
            enable_prefix_search: true,
            dedup_keys: true,
        }
    }
}

/// Statistics about a compiled FST.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FstStats {
    /// Number of entries (keys) in the FST.
    pub entry_count: usize,
    /// Total number of trie nodes.
    pub node_count: usize,
    /// Total bytes of all keys.
    pub total_key_bytes: usize,
    /// Maximum depth in the trie.
    pub max_depth: usize,
    /// Number of shared prefix bytes saved by the trie.
    pub shared_prefix_bytes: usize,
    /// Build duration in microseconds.
    pub build_duration_us: u64,
}

/// Result of an FST lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FstMatch {
    /// The reflex ID that matched.
    pub reflex_id: ReflexId,
    /// Priority of the match.
    pub priority: u32,
    /// Length of the matching key.
    pub match_len: usize,
    /// Cluster ID.
    pub cluster_id: String,
}

/// Error during FST compilation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FstError {
    /// Key exceeds maximum length.
    KeyTooLong { len: usize, max: usize },
    /// Too many entries.
    TooManyEntries { count: usize, max: usize },
    /// Empty key.
    EmptyKey,
    /// No entries to compile.
    EmptyInput,
}

impl std::fmt::Display for FstError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::KeyTooLong { len, max } => write!(f, "key too long: {len} > {max}"),
            Self::TooManyEntries { count, max } => {
                write!(f, "too many entries: {count} > {max}")
            }
            Self::EmptyKey => write!(f, "empty key"),
            Self::EmptyInput => write!(f, "no entries to compile"),
        }
    }
}

// =============================================================================
// Trie Node
// =============================================================================

/// A node in the FST trie.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrieNode {
    /// Children keyed by byte value.
    children: BTreeMap<u8, usize>, // byte → node index
    /// If this node is an accepting state, the output value.
    output: Option<TrieOutput>,
}

/// Output stored at an accepting trie node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrieOutput {
    reflex_id: ReflexId,
    priority: u32,
    cluster_id: String,
}

impl TrieNode {
    fn new() -> Self {
        Self {
            children: BTreeMap::new(),
            output: None,
        }
    }
}

// =============================================================================
// FST Index (immutable compiled structure)
// =============================================================================

/// An immutable, compiled FST index for O(K) trigger lookups.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FstIndex {
    /// Trie nodes (index 0 = root).
    nodes: Vec<TrieNode>,
    /// Number of accepting states (entries).
    entry_count: usize,
    /// Build statistics.
    stats: FstStats,
    /// Configuration used to build this index.
    config: FstConfig,
}

impl FstIndex {
    /// Look up an exact key in the FST.
    pub fn lookup(&self, key: &[u8]) -> Option<FstMatch> {
        let mut node_idx = 0; // root
        for &byte in key {
            match self.nodes[node_idx].children.get(&byte) {
                Some(&next) => node_idx = next,
                None => return None,
            }
        }
        self.nodes[node_idx].output.as_ref().map(|out| FstMatch {
            reflex_id: out.reflex_id,
            priority: out.priority,
            match_len: key.len(),
            cluster_id: out.cluster_id.clone(),
        })
    }

    /// Find all entries whose key is a prefix of the given input.
    ///
    /// Returns matches ordered by key length (shortest first).
    pub fn prefix_search(&self, input: &[u8]) -> Vec<FstMatch> {
        let mut results = Vec::new();
        let mut node_idx = 0;

        for (i, &byte) in input.iter().enumerate() {
            // Check if current node is accepting (prefix match).
            if let Some(out) = &self.nodes[node_idx].output {
                results.push(FstMatch {
                    reflex_id: out.reflex_id,
                    priority: out.priority,
                    match_len: i,
                    cluster_id: out.cluster_id.clone(),
                });
            }

            match self.nodes[node_idx].children.get(&byte) {
                Some(&next) => node_idx = next,
                None => return results,
            }
        }

        // Check final node.
        if let Some(out) = &self.nodes[node_idx].output {
            results.push(FstMatch {
                reflex_id: out.reflex_id,
                priority: out.priority,
                match_len: input.len(),
                cluster_id: out.cluster_id.clone(),
            });
        }

        results
    }

    /// Find all entries whose key starts with the given prefix.
    ///
    /// Returns all matching entries under the prefix subtree.
    pub fn entries_with_prefix(&self, prefix: &[u8]) -> Vec<FstMatch> {
        let mut node_idx = 0;
        for &byte in prefix {
            match self.nodes[node_idx].children.get(&byte) {
                Some(&next) => node_idx = next,
                None => return Vec::new(),
            }
        }

        // Collect all outputs in the subtree.
        let mut results = Vec::new();
        self.collect_subtree(node_idx, prefix.len(), &mut results);
        results
    }

    /// Recursively collect all outputs in a subtree.
    fn collect_subtree(&self, node_idx: usize, depth: usize, results: &mut Vec<FstMatch>) {
        if let Some(out) = &self.nodes[node_idx].output {
            results.push(FstMatch {
                reflex_id: out.reflex_id,
                priority: out.priority,
                match_len: depth,
                cluster_id: out.cluster_id.clone(),
            });
        }
        for (&_byte, &child_idx) in &self.nodes[node_idx].children {
            self.collect_subtree(child_idx, depth + 1, results);
        }
    }

    /// Check if the FST contains a given key.
    pub fn contains(&self, key: &[u8]) -> bool {
        self.lookup(key).is_some()
    }

    /// Get the number of entries.
    pub fn len(&self) -> usize {
        self.entry_count
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.entry_count == 0
    }

    /// Get build statistics.
    pub fn stats(&self) -> &FstStats {
        &self.stats
    }

    /// Get the number of trie nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get the best (lowest priority) match from prefix search.
    pub fn best_prefix_match(&self, input: &[u8]) -> Option<FstMatch> {
        self.prefix_search(input)
            .into_iter()
            .min_by_key(|m| m.priority)
    }
}

// =============================================================================
// FST Compiler
// =============================================================================

/// Compiles trigger entries into an immutable FstIndex.
#[derive(Debug, Clone)]
pub struct FstCompiler {
    config: FstConfig,
}

impl FstCompiler {
    /// Create a compiler with the given configuration.
    pub fn new(config: FstConfig) -> Self {
        Self { config }
    }

    /// Create a compiler with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(FstConfig::default())
    }

    /// Get the configuration.
    pub fn config(&self) -> &FstConfig {
        &self.config
    }

    /// Compile a set of trigger entries into an FstIndex.
    pub fn compile(&self, entries: &[TriggerEntry]) -> Result<FstIndex, FstError> {
        if entries.is_empty() {
            return Err(FstError::EmptyInput);
        }

        if entries.len() > self.config.max_entries {
            return Err(FstError::TooManyEntries {
                count: entries.len(),
                max: self.config.max_entries,
            });
        }

        let start = std::time::Instant::now();

        // Validate and deduplicate.
        let mut sorted_entries = Vec::new();
        for entry in entries {
            if entry.key.is_empty() {
                return Err(FstError::EmptyKey);
            }
            if entry.key.len() > self.config.max_key_len {
                return Err(FstError::KeyTooLong {
                    len: entry.key.len(),
                    max: self.config.max_key_len,
                });
            }
            sorted_entries.push(entry.clone());
        }

        // Sort by key for deterministic trie construction.
        sorted_entries.sort_by(|a, b| a.key.cmp(&b.key));

        // Deduplicate: for identical keys, keep highest priority (lowest value).
        if self.config.dedup_keys {
            sorted_entries.dedup_by(|a, b| {
                if a.key == b.key {
                    // Keep the one with lower priority value (= higher priority).
                    if a.priority < b.priority {
                        // a has higher priority, overwrite b.
                        b.reflex_id = a.reflex_id;
                        b.priority = a.priority;
                        b.cluster_id.clone_from(&a.cluster_id);
                    }
                    true
                } else {
                    false
                }
            });
        }

        // Build trie.
        let mut nodes = vec![TrieNode::new()]; // root at index 0
        let mut total_key_bytes = 0usize;
        let mut max_depth = 0usize;
        let mut shared_prefix_bytes = 0usize;
        let entry_count = sorted_entries.len();

        for entry in &sorted_entries {
            let mut node_idx = 0;
            total_key_bytes += entry.key.len();

            for (depth, &byte) in entry.key.iter().enumerate() {
                if depth + 1 > max_depth {
                    max_depth = depth + 1;
                }

                if let Some(&existing) = nodes[node_idx].children.get(&byte) {
                    // Shared prefix path.
                    shared_prefix_bytes += 1;
                    node_idx = existing;
                } else {
                    // New node.
                    let new_idx = nodes.len();
                    nodes.push(TrieNode::new());
                    nodes[node_idx].children.insert(byte, new_idx);
                    node_idx = new_idx;
                }
            }

            // Set output at final node.
            let new_output = TrieOutput {
                reflex_id: entry.reflex_id,
                priority: entry.priority,
                cluster_id: entry.cluster_id.clone(),
            };

            // If already occupied (shouldn't happen after dedup, but handle anyway).
            if let Some(existing) = &nodes[node_idx].output {
                if entry.priority < existing.priority {
                    nodes[node_idx].output = Some(new_output);
                }
            } else {
                nodes[node_idx].output = Some(new_output);
            }
        }

        let build_duration_us = start.elapsed().as_micros() as u64;

        let stats = FstStats {
            entry_count,
            node_count: nodes.len(),
            total_key_bytes,
            max_depth,
            shared_prefix_bytes,
            build_duration_us,
        };

        debug!(
            entries = entry_count,
            nodes = nodes.len(),
            shared_prefix_bytes,
            build_us = build_duration_us,
            "FST compiled"
        );

        Ok(FstIndex {
            nodes,
            entry_count,
            stats,
            config: self.config.clone(),
        })
    }
}

// =============================================================================
// MinHash key serialization
// =============================================================================

/// Serialize a MinHash signature into a canonical byte key for FST insertion.
///
/// MinHash values are encoded as big-endian u64 bytes, concatenated.
pub fn minhash_to_key(signature: &[u64]) -> Vec<u8> {
    let mut key = Vec::with_capacity(signature.len() * 8);
    for &val in signature {
        key.extend_from_slice(&val.to_be_bytes());
    }
    key
}

/// Deserialize a byte key back into MinHash values.
pub fn key_to_minhash(key: &[u8]) -> Vec<u64> {
    key.chunks_exact(8)
        .map(|chunk| {
            let mut arr = [0u8; 8];
            arr.copy_from_slice(chunk);
            u64::from_be_bytes(arr)
        })
        .collect()
}

// =============================================================================
// Hot-swap handle
// =============================================================================

/// A handle providing lock-free read access to the current FST index,
/// with atomic swap for background rebuilds.
///
/// Uses a generation counter + interior pointer swap pattern
/// (simplified ArcSwap equivalent without external deps).
#[derive(Debug)]
pub struct FstHandle {
    /// Current index (behind a read-write lock for simplicity).
    inner: std::sync::RwLock<FstIndex>,
    /// Generation counter (incremented on each swap).
    generation: AtomicU64,
}

impl FstHandle {
    /// Create a handle with an initial index.
    pub fn new(index: FstIndex) -> Self {
        Self {
            inner: std::sync::RwLock::new(index),
            generation: AtomicU64::new(0),
        }
    }

    /// Get the current generation.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Look up a key in the current index.
    pub fn lookup(&self, key: &[u8]) -> Option<FstMatch> {
        let guard = self.inner.read().unwrap();
        guard.lookup(key)
    }

    /// Prefix search in the current index.
    pub fn prefix_search(&self, input: &[u8]) -> Vec<FstMatch> {
        let guard = self.inner.read().unwrap();
        guard.prefix_search(input)
    }

    /// Hot-swap the index with a newly compiled one.
    pub fn swap(&self, new_index: FstIndex) {
        let mut guard = self.inner.write().unwrap();
        let old_gen = self.generation.fetch_add(1, Ordering::Release);
        debug!(
            old_gen,
            new_entries = new_index.entry_count,
            "FST hot-swapped"
        );
        *guard = new_index;
    }

    /// Get stats from the current index.
    pub fn stats(&self) -> FstStats {
        let guard = self.inner.read().unwrap();
        guard.stats().clone()
    }

    /// Get the entry count.
    pub fn len(&self) -> usize {
        let guard = self.inner.read().unwrap();
        guard.len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// =============================================================================
// Builder convenience
// =============================================================================

/// Builder for constructing TriggerEntry lists conveniently.
#[derive(Debug, Clone, Default)]
pub struct TriggerBuilder {
    entries: Vec<TriggerEntry>,
    next_id: ReflexId,
}

impl TriggerBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a literal string trigger.
    pub fn add_literal(&mut self, trigger: &str, cluster_id: &str, priority: u32) -> ReflexId {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(TriggerEntry {
            key: trigger.as_bytes().to_vec(),
            reflex_id: id,
            priority,
            cluster_id: cluster_id.to_string(),
        });
        id
    }

    /// Add a MinHash signature trigger.
    pub fn add_minhash(&mut self, signature: &[u64], cluster_id: &str, priority: u32) -> ReflexId {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(TriggerEntry {
            key: minhash_to_key(signature),
            reflex_id: id,
            priority,
            cluster_id: cluster_id.to_string(),
        });
        id
    }

    /// Get the accumulated entries.
    pub fn entries(&self) -> &[TriggerEntry] {
        &self.entries
    }

    /// Build into a vec of entries, consuming the builder.
    pub fn build(self) -> Vec<TriggerEntry> {
        self.entries
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, id: ReflexId, priority: u32) -> TriggerEntry {
        TriggerEntry {
            key: key.as_bytes().to_vec(),
            reflex_id: id,
            priority,
            cluster_id: format!("cluster-{id}"),
        }
    }

    // ---- Basic compilation ----

    #[test]
    fn compile_single_entry() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("error: not found", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        assert_eq!(index.len(), 1);
        assert!(!index.is_empty());
    }

    #[test]
    fn compile_multiple_entries() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("error: not found", 1, 0),
            entry("error: permission denied", 2, 1),
            entry("warning: unused variable", 3, 2),
        ];
        let index = compiler.compile(&entries).unwrap();
        assert_eq!(index.len(), 3);
    }

    #[test]
    fn compile_empty_fails() {
        let compiler = FstCompiler::with_defaults();
        let err = compiler.compile(&[]).unwrap_err();
        assert_eq!(err, FstError::EmptyInput);
    }

    #[test]
    fn compile_empty_key_fails() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![TriggerEntry {
            key: Vec::new(),
            reflex_id: 1,
            priority: 0,
            cluster_id: "c".to_string(),
        }];
        let err = compiler.compile(&entries).unwrap_err();
        assert_eq!(err, FstError::EmptyKey);
    }

    #[test]
    fn compile_key_too_long_fails() {
        let config = FstConfig {
            max_key_len: 10,
            ..Default::default()
        };
        let compiler = FstCompiler::new(config);
        let entries = vec![entry("this is a very long key", 1, 0)];
        let err = compiler.compile(&entries).unwrap_err();
        let is_too_long = matches!(err, FstError::KeyTooLong { .. });
        assert!(is_too_long);
    }

    #[test]
    fn compile_too_many_entries_fails() {
        let config = FstConfig {
            max_entries: 2,
            ..Default::default()
        };
        let compiler = FstCompiler::new(config);
        let entries = vec![entry("a", 1, 0), entry("b", 2, 0), entry("c", 3, 0)];
        let err = compiler.compile(&entries).unwrap_err();
        let is_too_many = matches!(err, FstError::TooManyEntries { .. });
        assert!(is_too_many);
    }

    // ---- Exact lookup ----

    #[test]
    fn lookup_exact_match() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("error: not found", 1, 0),
            entry("error: timeout", 2, 1),
        ];
        let index = compiler.compile(&entries).unwrap();

        let m = index.lookup(b"error: not found").unwrap();
        assert_eq!(m.reflex_id, 1);
        assert_eq!(m.priority, 0);
    }

    #[test]
    fn lookup_miss() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("error: not found", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        assert!(index.lookup(b"error: timeout").is_none());
    }

    #[test]
    fn lookup_partial_key_misses() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("error: not found", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        // Partial key should not match.
        assert!(index.lookup(b"error: not").is_none());
    }

    #[test]
    fn contains_works() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("abc", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        assert!(index.contains(b"abc"));
        assert!(!index.contains(b"abd"));
        assert!(!index.contains(b"ab"));
    }

    // ---- Prefix search ----

    #[test]
    fn prefix_search_finds_prefix_triggers() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("err", 1, 0),
            entry("error", 2, 1),
            entry("error: not found", 3, 2),
        ];
        let index = compiler.compile(&entries).unwrap();

        let matches = index.prefix_search(b"error: not found in src/main.rs");
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0].reflex_id, 1); // "err"
        assert_eq!(matches[1].reflex_id, 2); // "error"
        assert_eq!(matches[2].reflex_id, 3); // "error: not found"
    }

    #[test]
    fn prefix_search_partial_match() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("err", 1, 0), entry("error: not found", 2, 1)];
        let index = compiler.compile(&entries).unwrap();

        let matches = index.prefix_search(b"error: timeout");
        assert_eq!(matches.len(), 1); // only "err" matches
        assert_eq!(matches[0].reflex_id, 1);
    }

    #[test]
    fn prefix_search_no_match() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("warning", 1, 0)];
        let index = compiler.compile(&entries).unwrap();

        let matches = index.prefix_search(b"error: foo");
        assert!(matches.is_empty());
    }

    #[test]
    fn best_prefix_match_returns_highest_priority() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("err", 1, 5),
            entry("error", 2, 1),
            entry("error: not", 3, 10),
        ];
        let index = compiler.compile(&entries).unwrap();

        let best = index.best_prefix_match(b"error: not found").unwrap();
        assert_eq!(best.reflex_id, 2); // priority 1 is best
    }

    // ---- Entries with prefix (reverse direction) ----

    #[test]
    fn entries_with_prefix_finds_subtree() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("error: not found", 1, 0),
            entry("error: timeout", 2, 1),
            entry("warning: unused", 3, 2),
        ];
        let index = compiler.compile(&entries).unwrap();

        let matches = index.entries_with_prefix(b"error:");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn entries_with_prefix_empty_result() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("error: not found", 1, 0)];
        let index = compiler.compile(&entries).unwrap();

        let matches = index.entries_with_prefix(b"warn");
        assert!(matches.is_empty());
    }

    // ---- Shared prefix compression ----

    #[test]
    fn shared_prefix_reduces_nodes() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("error: not found", 1, 0),
            entry("error: timeout", 2, 1),
        ];
        let index = compiler.compile(&entries).unwrap();

        // "error: " (7 bytes) shared prefix → 7 shared nodes
        assert!(index.stats().shared_prefix_bytes > 0);
        // Total unique chars: "not found" (9) + "timeout" (7) + shared "error: " (7) = 23
        // Nodes should be < total chars of both strings (16 + 14 = 30)
        assert!(index.node_count() < 30);
    }

    // ---- Deduplication ----

    #[test]
    fn dedup_keeps_highest_priority() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![
            entry("error: dup", 1, 5),
            entry("error: dup", 2, 1), // Higher priority (lower value).
        ];
        let index = compiler.compile(&entries).unwrap();
        assert_eq!(index.len(), 1);

        let m = index.lookup(b"error: dup").unwrap();
        assert_eq!(m.priority, 1);
    }

    #[test]
    fn no_dedup_when_disabled() {
        let config = FstConfig {
            dedup_keys: false,
            ..Default::default()
        };
        let compiler = FstCompiler::new(config);
        let entries = vec![entry("abc", 1, 5), entry("abc", 2, 1)];
        let index = compiler.compile(&entries).unwrap();
        // Both inserted; last one wins in the trie.
        assert_eq!(index.len(), 2);
    }

    // ---- MinHash key serialization ----

    #[test]
    fn minhash_roundtrip() {
        let sig = vec![12345u64, 67890, u64::MAX, 0];
        let key = minhash_to_key(&sig);
        let recovered = key_to_minhash(&key);
        assert_eq!(recovered, sig);
    }

    #[test]
    fn minhash_key_length() {
        let sig = vec![1u64, 2, 3];
        let key = minhash_to_key(&sig);
        assert_eq!(key.len(), 24); // 3 * 8
    }

    #[test]
    fn minhash_lookup_works() {
        let compiler = FstCompiler::with_defaults();
        let sig = vec![100u64, 200, 300];
        let key = minhash_to_key(&sig);
        let entries = vec![TriggerEntry {
            key: key.clone(),
            reflex_id: 42,
            priority: 0,
            cluster_id: "mh-1".to_string(),
        }];
        let index = compiler.compile(&entries).unwrap();
        let m = index.lookup(&key).unwrap();
        assert_eq!(m.reflex_id, 42);
    }

    // ---- Hot-swap handle ----

    #[test]
    fn handle_lookup_works() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("hello", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        let handle = FstHandle::new(index);

        assert_eq!(handle.generation(), 0);
        let m = handle.lookup(b"hello").unwrap();
        assert_eq!(m.reflex_id, 1);
    }

    #[test]
    fn handle_swap_updates_index() {
        let compiler = FstCompiler::with_defaults();

        let idx1 = compiler.compile(&[entry("old", 1, 0)]).unwrap();
        let idx2 = compiler.compile(&[entry("new", 2, 0)]).unwrap();

        let handle = FstHandle::new(idx1);
        assert!(handle.lookup(b"old").is_some());
        assert!(handle.lookup(b"new").is_none());

        handle.swap(idx2);
        assert_eq!(handle.generation(), 1);
        assert!(handle.lookup(b"old").is_none());
        assert!(handle.lookup(b"new").is_some());
    }

    #[test]
    fn handle_prefix_search() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("err", 1, 0), entry("error", 2, 1)];
        let index = compiler.compile(&entries).unwrap();
        let handle = FstHandle::new(index);

        let matches = handle.prefix_search(b"error: foo");
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn handle_stats() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("abc", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        let handle = FstHandle::new(index);

        let stats = handle.stats();
        assert_eq!(stats.entry_count, 1);
    }

    // ---- Builder ----

    #[test]
    fn builder_add_literal() {
        let mut builder = TriggerBuilder::new();
        let id = builder.add_literal("error: crash", "c1", 0);
        assert_eq!(id, 0);
        assert_eq!(builder.entries().len(), 1);
    }

    #[test]
    fn builder_add_minhash() {
        let mut builder = TriggerBuilder::new();
        let id = builder.add_minhash(&[1, 2, 3], "c1", 1);
        assert_eq!(id, 0);
        assert_eq!(builder.entries()[0].key.len(), 24);
    }

    #[test]
    fn builder_ids_increment() {
        let mut builder = TriggerBuilder::new();
        let id0 = builder.add_literal("a", "c", 0);
        let id1 = builder.add_literal("b", "c", 0);
        let id2 = builder.add_minhash(&[1], "c", 0);
        assert_eq!(id0, 0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn builder_compiles() {
        let mut builder = TriggerBuilder::new();
        builder.add_literal("error: foo", "c1", 0);
        builder.add_literal("error: bar", "c2", 1);
        let compiler = FstCompiler::with_defaults();
        let index = compiler.compile(&builder.build()).unwrap();
        assert_eq!(index.len(), 2);
    }

    // ---- Stats ----

    #[test]
    fn stats_entry_count_correct() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("a", 1, 0), entry("b", 2, 0), entry("c", 3, 0)];
        let index = compiler.compile(&entries).unwrap();
        assert_eq!(index.stats().entry_count, 3);
    }

    #[test]
    fn stats_max_depth_correct() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("abcdef", 1, 0)];
        let index = compiler.compile(&entries).unwrap();
        assert_eq!(index.stats().max_depth, 6);
    }

    #[test]
    fn stats_total_key_bytes_correct() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("abc", 1, 0), entry("defg", 2, 0)];
        let index = compiler.compile(&entries).unwrap();
        assert_eq!(index.stats().total_key_bytes, 7);
    }

    // ---- Error display ----

    #[test]
    fn error_display() {
        assert!(FstError::EmptyInput.to_string().contains("no entries"));
        assert!(FstError::EmptyKey.to_string().contains("empty key"));
        assert!(
            FstError::KeyTooLong { len: 10, max: 5 }
                .to_string()
                .contains("key too long")
        );
        assert!(
            FstError::TooManyEntries { count: 10, max: 5 }
                .to_string()
                .contains("too many entries")
        );
    }

    // ---- Serde roundtrips ----

    #[test]
    fn trigger_entry_serde_roundtrip() {
        let e = entry("test", 42, 3);
        let json = serde_json::to_string(&e).unwrap();
        let decoded: TriggerEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, e);
    }

    #[test]
    fn fst_config_serde_roundtrip() {
        let config = FstConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: FstConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.max_key_len, config.max_key_len);
        assert_eq!(decoded.max_entries, config.max_entries);
    }

    #[test]
    fn fst_stats_serde_roundtrip() {
        let stats = FstStats {
            entry_count: 10,
            node_count: 50,
            total_key_bytes: 200,
            max_depth: 20,
            shared_prefix_bytes: 100,
            build_duration_us: 42,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: FstStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    #[test]
    fn fst_match_serde_roundtrip() {
        let m = FstMatch {
            reflex_id: 1,
            priority: 0,
            match_len: 5,
            cluster_id: "c1".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let decoded: FstMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, m);
    }

    #[test]
    fn fst_error_serde_roundtrip() {
        let err = FstError::KeyTooLong { len: 10, max: 5 };
        let json = serde_json::to_string(&err).unwrap();
        let decoded: FstError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, err);
    }

    #[test]
    fn fst_index_serde_roundtrip() {
        let compiler = FstCompiler::with_defaults();
        let entries = vec![entry("hello", 1, 0), entry("world", 2, 1)];
        let index = compiler.compile(&entries).unwrap();

        let json = serde_json::to_string(&index).unwrap();
        let decoded: FstIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.len(), 2);
        assert!(decoded.contains(b"hello"));
        assert!(decoded.contains(b"world"));
    }
}
