//! Sharded concurrent hash map — low-contention pane registry
//!
//! Replaces `RwLock<HashMap<u64, T>>` with a cache-line-padded, sharded
//! map that distributes entries across independent lock shards. With 64
//! shards and 200 panes, each shard holds ~3 entries — read contention
//! is effectively zero.
//!
//! # Design
//!
//! Each shard is a `RwLock<HashMap<K, V>>` padded to 128 bytes (one
//! Apple Silicon cache line / two x86_64 cache lines). Key hashing
//! determines the shard, so operations on different panes never touch
//! the same lock.
//!
//! # When to Use
//!
//! Use [`ShardedMap`] for read-heavy maps on hot paths (pane registry,
//! cursor lookup, tier classification). For single-writer / many-reader
//! patterns where entries are rarely added/removed, this provides near-
//! lock-free read performance.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default number of shards. Power of 2 for fast modulo.
const DEFAULT_SHARDS: usize = 64;

// ---------------------------------------------------------------------------
// Shard key hashing
// ---------------------------------------------------------------------------

/// Fast hash for shard selection (splitmix64 finalizer).
///
/// Uses the splitmix64 bit mixer for excellent avalanche on sequential keys.
#[inline]
fn fx_hash_u64(key: u64) -> usize {
    let mut h = key;
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    h as usize
}

/// Generic hash for non-u64 keys.
#[inline]
fn hash_key<K: Hash>(key: &K, shard_count: usize) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    (hasher.finish() as usize) % shard_count
}

// ---------------------------------------------------------------------------
// Padded shard
// ---------------------------------------------------------------------------

/// A single shard: a padded RwLock<HashMap>.
///
/// 128-byte alignment prevents false sharing between adjacent shards.
#[repr(align(128))]
struct Shard<K, V> {
    map: RwLock<HashMap<K, V>>,
}

impl<K, V> Shard<K, V> {
    fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for Shard<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.map.read() {
            Ok(guard) => f.debug_struct("Shard").field("entries", &guard.len()).finish(),
            Err(_) => f.debug_struct("Shard").field("entries", &"<poisoned>").finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// ShardedMap
// ---------------------------------------------------------------------------

/// A sharded concurrent hash map optimized for read-heavy workloads.
///
/// Thread-safe without external locking. Operations on different keys
/// are fully concurrent (no shared lock).
///
/// # Type Parameters
///
/// * `K` — key type (must be `Hash + Eq + Clone`)
/// * `V` — value type
pub struct ShardedMap<K, V> {
    shards: Box<[Shard<K, V>]>,
    shard_count: usize,
}

impl<K: std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for ShardedMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardedMap")
            .field("shard_count", &self.shard_count)
            .field("shards", &self.shards)
            .finish()
    }
}

impl<K, V> ShardedMap<K, V>
where
    K: Hash + Eq + Clone,
{
    /// Create a new sharded map with the default shard count (64).
    #[must_use]
    pub fn new() -> Self {
        Self::with_shards(DEFAULT_SHARDS)
    }

    /// Create with a specific shard count.
    ///
    /// Clamped to `[1, 256]`.
    #[must_use]
    pub fn with_shards(n: usize) -> Self {
        let n = n.clamp(1, 256);
        let shards: Vec<Shard<K, V>> = (0..n).map(|_| Shard::new()).collect();
        Self {
            shards: shards.into_boxed_slice(),
            shard_count: n,
        }
    }

    /// Number of shards.
    #[must_use]
    pub fn shard_count(&self) -> usize {
        self.shard_count
    }

    /// Resolve key to shard index.
    #[inline]
    fn shard_idx(&self, key: &K) -> usize {
        hash_key(key, self.shard_count)
    }

    /// Insert or update a key-value pair. Returns the old value if any.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let idx = self.shard_idx(&key);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(key, value)
    }

    /// Get a clone of the value for a key.
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let idx = self.shard_idx(key);
        let guard = self.shards[idx]
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(key).cloned()
    }

    /// Check if the map contains a key.
    pub fn contains_key(&self, key: &K) -> bool {
        let idx = self.shard_idx(key);
        let guard = self.shards[idx]
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.contains_key(key)
    }

    /// Remove a key and return its value.
    pub fn remove(&self, key: &K) -> Option<V> {
        let idx = self.shard_idx(key);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove(key)
    }

    /// Total number of entries across all shards.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                s.map
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .len()
            })
            .sum()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| {
            s.map
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        })
    }

    /// Apply a function to a value under a read lock.
    ///
    /// Returns `None` if the key doesn't exist.
    pub fn read_with<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&V) -> R,
    {
        let idx = self.shard_idx(key);
        let guard = self.shards[idx]
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(key).map(f)
    }

    /// Apply a mutating function to a value under a write lock.
    ///
    /// Returns `None` if the key doesn't exist.
    pub fn write_with<F, R>(&self, key: &K, f: F) -> Option<R>
    where
        F: FnOnce(&mut V) -> R,
    {
        let idx = self.shard_idx(key);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.get_mut(key).map(f)
    }

    /// Insert if absent, returning a reference to the (possibly existing) value.
    ///
    /// Returns `true` if a new entry was inserted, `false` if it already existed.
    pub fn insert_if_absent(&self, key: K, value: V) -> bool {
        let idx = self.shard_idx(&key);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(&key) {
            false
        } else {
            guard.insert(key, value);
            true
        }
    }

    /// Collect all keys (snapshot).
    pub fn keys(&self) -> Vec<K> {
        let mut result = Vec::new();
        for shard in self.shards.iter() {
            let guard = shard.map.read().unwrap_or_else(|e| e.into_inner());
            result.extend(guard.keys().cloned());
        }
        result
    }

    /// Collect all values (snapshot).
    pub fn values(&self) -> Vec<V>
    where
        V: Clone,
    {
        let mut result = Vec::new();
        for shard in self.shards.iter() {
            let guard = shard.map.read().unwrap_or_else(|e| e.into_inner());
            result.extend(guard.values().cloned());
        }
        result
    }

    /// Collect all key-value pairs (snapshot).
    pub fn entries(&self) -> Vec<(K, V)>
    where
        V: Clone,
    {
        let mut result = Vec::new();
        for shard in self.shards.iter() {
            let guard = shard.map.read().unwrap_or_else(|e| e.into_inner());
            for (k, v) in guard.iter() {
                result.push((k.clone(), v.clone()));
            }
        }
        result
    }

    /// Retain only entries satisfying a predicate.
    pub fn retain<F>(&self, mut f: F)
    where
        F: FnMut(&K, &V) -> bool,
    {
        for shard in self.shards.iter() {
            let mut guard = shard
                .map
                .write()
                .unwrap_or_else(|e| e.into_inner());
            guard.retain(|k, v| f(k, v));
        }
    }

    /// Per-shard entry counts (for diagnostics).
    pub fn shard_sizes(&self) -> Vec<usize> {
        self.shards
            .iter()
            .map(|s| {
                s.map
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .len()
            })
            .collect()
    }

    /// Clear all entries.
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            let mut guard = shard
                .map
                .write()
                .unwrap_or_else(|e| e.into_inner());
            guard.clear();
        }
    }
}

impl<K: Hash + Eq + Clone, V> Default for ShardedMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Specialized u64-keyed variant for pane IDs
// ---------------------------------------------------------------------------

/// A sharded map optimized for `u64` keys (pane IDs).
///
/// Uses Fibonacci hashing for excellent distribution of sequential IDs.
pub struct PaneMap<V> {
    shards: Box<[Shard<u64, V>]>,
    shard_count: usize,
}

impl<V: std::fmt::Debug> std::fmt::Debug for PaneMap<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneMap")
            .field("shard_count", &self.shard_count)
            .finish()
    }
}

impl<V> PaneMap<V> {
    /// Create with default shard count.
    #[must_use]
    pub fn new() -> Self {
        Self::with_shards(DEFAULT_SHARDS)
    }

    /// Create with specific shard count.
    #[must_use]
    pub fn with_shards(n: usize) -> Self {
        let n = n.clamp(1, 256);
        let shards: Vec<Shard<u64, V>> = (0..n).map(|_| Shard::new()).collect();
        Self {
            shards: shards.into_boxed_slice(),
            shard_count: n,
        }
    }

    #[inline]
    fn shard_idx(&self, pane_id: u64) -> usize {
        fx_hash_u64(pane_id) % self.shard_count
    }

    /// Insert a pane entry.
    pub fn insert(&self, pane_id: u64, value: V) -> Option<V> {
        let idx = self.shard_idx(pane_id);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.insert(pane_id, value)
    }

    /// Get a cloned value.
    pub fn get(&self, pane_id: u64) -> Option<V>
    where
        V: Clone,
    {
        let idx = self.shard_idx(pane_id);
        let guard = self.shards[idx]
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(&pane_id).cloned()
    }

    /// Check if a pane exists.
    pub fn contains(&self, pane_id: u64) -> bool {
        let idx = self.shard_idx(pane_id);
        let guard = self.shards[idx]
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.contains_key(&pane_id)
    }

    /// Remove a pane entry.
    pub fn remove(&self, pane_id: u64) -> Option<V> {
        let idx = self.shard_idx(pane_id);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove(&pane_id)
    }

    /// Read a value with a closure (avoids clone).
    pub fn read_with<F, R>(&self, pane_id: u64, f: F) -> Option<R>
    where
        F: FnOnce(&V) -> R,
    {
        let idx = self.shard_idx(pane_id);
        let guard = self.shards[idx]
            .map
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.get(&pane_id).map(f)
    }

    /// Mutate a value in place.
    pub fn write_with<F, R>(&self, pane_id: u64, f: F) -> Option<R>
    where
        F: FnOnce(&mut V) -> R,
    {
        let idx = self.shard_idx(pane_id);
        let mut guard = self.shards[idx]
            .map
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.get_mut(&pane_id).map(f)
    }

    /// Total entries.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|s| {
                s.map
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .len()
            })
            .sum()
    }

    /// Whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| {
            s.map
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        })
    }

    /// All pane IDs.
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut result = Vec::new();
        for shard in self.shards.iter() {
            let guard = shard.map.read().unwrap_or_else(|e| e.into_inner());
            result.extend(guard.keys());
        }
        result
    }

    /// Retain only entries matching a predicate.
    pub fn retain<F>(&self, mut f: F)
    where
        F: FnMut(u64, &V) -> bool,
    {
        for shard in self.shards.iter() {
            let mut guard = shard
                .map
                .write()
                .unwrap_or_else(|e| e.into_inner());
            guard.retain(|k, v| f(*k, v));
        }
    }

    /// Per-shard entry counts.
    pub fn shard_sizes(&self) -> Vec<usize> {
        self.shards
            .iter()
            .map(|s| {
                s.map
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .len()
            })
            .collect()
    }

    /// Clear all entries.
    pub fn clear(&self) {
        for shard in self.shards.iter() {
            let mut guard = shard
                .map
                .write()
                .unwrap_or_else(|e| e.into_inner());
            guard.clear();
        }
    }
}

impl<V> Default for PaneMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Distribution quality metric
// ---------------------------------------------------------------------------

/// Measure the quality of key distribution across shards.
///
/// Returns (min_entries, max_entries, stddev). A good hash function yields
/// low stddev relative to the mean.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributionStats {
    pub shard_count: usize,
    pub total_entries: usize,
    pub min_shard_size: usize,
    pub max_shard_size: usize,
    pub mean_shard_size: f64,
    pub stddev_shard_size: f64,
}

impl DistributionStats {
    /// Compute distribution stats from shard sizes.
    #[must_use]
    pub fn from_shard_sizes(sizes: &[usize]) -> Self {
        let total: usize = sizes.iter().sum();
        let mean = if sizes.is_empty() {
            0.0
        } else {
            total as f64 / sizes.len() as f64
        };
        let variance = if sizes.is_empty() {
            0.0
        } else {
            sizes.iter().map(|&s| (s as f64 - mean).powi(2)).sum::<f64>() / sizes.len() as f64
        };

        Self {
            shard_count: sizes.len(),
            total_entries: total,
            min_shard_size: sizes.iter().copied().min().unwrap_or(0),
            max_shard_size: sizes.iter().copied().max().unwrap_or(0),
            mean_shard_size: mean,
            stddev_shard_size: variance.sqrt(),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // -----------------------------------------------------------------------
    // ShardedMap basic operations
    // -----------------------------------------------------------------------

    #[test]
    fn map_insert_get() {
        let map: ShardedMap<String, i32> = ShardedMap::with_shards(8);
        assert!(map.is_empty());

        map.insert("foo".to_string(), 42);
        assert_eq!(map.get(&"foo".to_string()), Some(42));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn map_insert_overwrite() {
        let map: ShardedMap<String, i32> = ShardedMap::with_shards(8);
        map.insert("foo".to_string(), 1);
        let old = map.insert("foo".to_string(), 2);
        assert_eq!(old, Some(1));
        assert_eq!(map.get(&"foo".to_string()), Some(2));
    }

    #[test]
    fn map_remove() {
        let map: ShardedMap<String, i32> = ShardedMap::with_shards(8);
        map.insert("foo".to_string(), 42);
        let removed = map.remove(&"foo".to_string());
        assert_eq!(removed, Some(42));
        assert!(map.is_empty());
    }

    #[test]
    fn map_contains_key() {
        let map: ShardedMap<String, i32> = ShardedMap::with_shards(8);
        map.insert("foo".to_string(), 42);
        assert!(map.contains_key(&"foo".to_string()));
        assert!(!map.contains_key(&"bar".to_string()));
    }

    #[test]
    fn map_read_with() {
        let map: ShardedMap<String, Vec<i32>> = ShardedMap::with_shards(8);
        map.insert("foo".to_string(), vec![1, 2, 3]);
        let len = map.read_with(&"foo".to_string(), |v| v.len());
        assert_eq!(len, Some(3));
    }

    #[test]
    fn map_write_with() {
        let map: ShardedMap<String, Vec<i32>> = ShardedMap::with_shards(8);
        map.insert("foo".to_string(), vec![1, 2, 3]);
        map.write_with(&"foo".to_string(), |v| v.push(4));
        let len = map.read_with(&"foo".to_string(), |v| v.len());
        assert_eq!(len, Some(4));
    }

    #[test]
    fn map_insert_if_absent() {
        let map: ShardedMap<String, i32> = ShardedMap::with_shards(8);
        assert!(map.insert_if_absent("foo".to_string(), 1));
        assert!(!map.insert_if_absent("foo".to_string(), 2));
        assert_eq!(map.get(&"foo".to_string()), Some(1)); // original preserved
    }

    #[test]
    fn map_keys_values_entries() {
        let map: ShardedMap<u32, String> = ShardedMap::with_shards(4);
        map.insert(1, "a".to_string());
        map.insert(2, "b".to_string());
        map.insert(3, "c".to_string());

        let mut keys = map.keys();
        keys.sort();
        assert_eq!(keys, vec![1, 2, 3]);
        assert_eq!(map.values().len(), 3);
        assert_eq!(map.entries().len(), 3);
    }

    #[test]
    fn map_retain() {
        let map: ShardedMap<u32, i32> = ShardedMap::with_shards(4);
        for i in 0..10 {
            map.insert(i, i as i32);
        }
        map.retain(|_, v| *v % 2 == 0);
        assert_eq!(map.len(), 5); // 0, 2, 4, 6, 8
    }

    #[test]
    fn map_clear() {
        let map: ShardedMap<u32, i32> = ShardedMap::with_shards(4);
        for i in 0..10 {
            map.insert(i, i as i32);
        }
        map.clear();
        assert!(map.is_empty());
    }

    // -----------------------------------------------------------------------
    // ShardedMap concurrency
    // -----------------------------------------------------------------------

    #[test]
    fn map_concurrent_inserts() {
        let map = Arc::new(ShardedMap::<u64, u64>::with_shards(16));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for j in 0..1_000u64 {
                        map.insert(i * 1000 + j, j);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(map.len(), 8000);
    }

    #[test]
    fn map_concurrent_reads_and_writes() {
        let map = Arc::new(ShardedMap::<u64, u64>::with_shards(16));

        // Pre-populate
        for i in 0..100 {
            map.insert(i, i);
        }

        let threads: Vec<_> = (0..16)
            .map(|i| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for j in 0..10_000u64 {
                        if j % 10 == 0 {
                            // 10% writes
                            map.insert(i * 1000 + j, j);
                        } else {
                            // 90% reads
                            let _ = map.get(&(j % 100));
                        }
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        // All pre-populated keys still exist
        for i in 0..100 {
            assert!(map.contains_key(&i));
        }
    }

    // -----------------------------------------------------------------------
    // PaneMap
    // -----------------------------------------------------------------------

    #[test]
    fn pane_map_basic() {
        let map = PaneMap::<String>::with_shards(16);
        map.insert(1, "pane-1".to_string());
        map.insert(2, "pane-2".to_string());
        assert_eq!(map.get(1), Some("pane-1".to_string()));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn pane_map_remove() {
        let map = PaneMap::<i32>::with_shards(16);
        map.insert(42, 100);
        assert_eq!(map.remove(42), Some(100));
        assert!(map.is_empty());
    }

    #[test]
    fn pane_map_read_write_with() {
        let map = PaneMap::<Vec<u8>>::with_shards(8);
        map.insert(1, vec![10, 20]);
        map.write_with(1, |v| v.push(30));
        let len = map.read_with(1, |v| v.len());
        assert_eq!(len, Some(3));
    }

    #[test]
    fn pane_map_pane_ids() {
        let map = PaneMap::<()>::with_shards(8);
        for i in 0..50 {
            map.insert(i, ());
        }
        let mut ids = map.pane_ids();
        ids.sort();
        assert_eq!(ids, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn pane_map_retain() {
        let map = PaneMap::<u64>::with_shards(8);
        for i in 0..20 {
            map.insert(i, i * 10);
        }
        map.retain(|id, _| id % 3 == 0);
        // Kept: 0, 3, 6, 9, 12, 15, 18 = 7 entries
        assert_eq!(map.len(), 7);
    }

    #[test]
    fn pane_map_concurrent() {
        let map = Arc::new(PaneMap::<u64>::with_shards(32));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for j in 0..500u64 {
                        let id = i * 500 + j;
                        map.insert(id, id * 2);
                    }
                })
            })
            .collect();

        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(map.len(), 4000);

        // Verify all entries
        for i in 0..4000u64 {
            assert_eq!(map.get(i), Some(i * 2));
        }
    }

    // -----------------------------------------------------------------------
    // Distribution quality
    // -----------------------------------------------------------------------

    #[test]
    fn pane_map_sequential_ids_distribute_well() {
        let map = PaneMap::<()>::with_shards(64);
        // Insert 200 sequential pane IDs (realistic scenario)
        for i in 0..200 {
            map.insert(i, ());
        }
        let sizes = map.shard_sizes();
        let stats = DistributionStats::from_shard_sizes(&sizes);

        // Mean should be ~3.125 (200/64)
        assert!(
            (stats.mean_shard_size - 3.125).abs() < 0.01,
            "mean={}, expected ~3.125",
            stats.mean_shard_size
        );
        // Max shard should hold at most ~2x the mean
        assert!(
            stats.max_shard_size <= 8,
            "max_shard_size={}, should be <= 8 for good distribution",
            stats.max_shard_size
        );
    }

    #[test]
    fn fx_hash_distribution() {
        // Verify splitmix64 distributes sequential IDs well.
        // Use 6400 samples → 100 per bucket (enough for stable chi-squared).
        let n_buckets = 64;
        let n_samples = 6400u64;
        let mut counts = vec![0usize; n_buckets];
        for i in 0..n_samples {
            let idx = fx_hash_u64(i) % n_buckets;
            counts[idx] += 1;
        }
        let expected = n_samples as f64 / n_buckets as f64; // 100.0
        let chi2: f64 = counts
            .iter()
            .map(|&c| {
                let diff = c as f64 - expected;
                diff * diff / expected
            })
            .sum();
        // Chi-squared with 63 df: p<0.001 critical value ≈ 100.4
        // A good hash should be well below this.
        assert!(
            chi2 < 120.0,
            "chi2={chi2:.1} exceeds threshold — poor distribution. counts: {counts:?}"
        );
    }

    // -----------------------------------------------------------------------
    // DistributionStats
    // -----------------------------------------------------------------------

    #[test]
    fn distribution_stats_from_sizes() {
        let sizes = vec![3, 3, 4, 2];
        let stats = DistributionStats::from_shard_sizes(&sizes);
        assert_eq!(stats.total_entries, 12);
        assert_eq!(stats.min_shard_size, 2);
        assert_eq!(stats.max_shard_size, 4);
        assert!((stats.mean_shard_size - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn distribution_stats_empty() {
        let stats = DistributionStats::from_shard_sizes(&[]);
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.min_shard_size, 0);
    }

    #[test]
    fn distribution_stats_serde_roundtrip() {
        let stats = DistributionStats {
            shard_count: 64,
            total_entries: 200,
            min_shard_size: 2,
            max_shard_size: 5,
            mean_shard_size: 3.125,
            stddev_shard_size: 0.8,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: DistributionStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back.shard_count, 64);
        assert_eq!(back.total_entries, 200);
    }
}
