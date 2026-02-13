//! Consistent hash ring with virtual nodes for even key distribution.
//!
//! Uses a sorted array of virtual node positions on a 64-bit hash ring.
//! Each physical node maps to multiple virtual nodes for balanced distribution.
//! Adding/removing a node only remaps ~1/N of keys (N = number of nodes).
//!
//! # Use Cases
//! - Distributing pane monitoring across worker threads
//! - Load balancing capture work across processing pipelines
//! - Assigning panes to storage shards
//!
//! # Example
//! ```
//! use frankenterm_core::consistent_hash::HashRing;
//!
//! let mut ring = HashRing::new(150); // 150 virtual nodes per physical node
//! ring.add_node("worker-0");
//! ring.add_node("worker-1");
//! ring.add_node("worker-2");
//!
//! let node = ring.get_node("pane-42").unwrap();
//! // node is one of "worker-0", "worker-1", "worker-2"
//!
//! // Get N replicas for redundancy
//! let replicas = ring.get_nodes("pane-42", 2);
//! assert_eq!(replicas.len(), 2);
//! ```

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

/// A consistent hash ring mapping keys to nodes using virtual nodes.
///
/// The ring uses FNV-1a hashing (same family as bloom_filter.rs) for fast,
/// well-distributed hashing. Virtual nodes ensure even distribution even
/// with a small number of physical nodes.
#[derive(Debug, Clone)]
pub struct HashRing<N: Clone + Eq + Hash> {
    /// Sorted map of hash position → (node, virtual_index).
    ring: BTreeMap<u64, (N, u32)>,
    /// Physical node → set of ring positions it occupies.
    node_positions: HashMap<N, Vec<u64>>,
    /// Number of virtual nodes per physical node.
    vnodes: u32,
}

/// Statistics about the hash ring.
#[derive(Debug, Clone)]
pub struct RingStats {
    /// Number of physical nodes.
    pub node_count: usize,
    /// Number of virtual nodes on the ring.
    pub vnode_count: usize,
    /// Virtual nodes per physical node.
    pub vnodes_per_node: u32,
    /// Standard deviation of key distribution (lower = more even).
    /// Computed by simulating key distribution across 10000 sample keys.
    pub distribution_stddev: f64,
    /// The min/max fraction of keys assigned to any single node.
    pub min_fraction: f64,
    pub max_fraction: f64,
}

impl<N: Clone + Eq + Hash + std::fmt::Debug> HashRing<N> {
    /// Create a new empty hash ring.
    ///
    /// `vnodes_per_node` controls how many virtual nodes each physical node
    /// gets on the ring. Higher values = more even distribution but more memory.
    /// Typical values: 100-200 for good balance.
    ///
    /// # Panics
    /// Panics if `vnodes_per_node` is 0.
    pub fn new(vnodes_per_node: u32) -> Self {
        assert!(vnodes_per_node > 0, "vnodes_per_node must be > 0");
        Self {
            ring: BTreeMap::new(),
            node_positions: HashMap::new(),
            vnodes: vnodes_per_node,
        }
    }

    /// Create a hash ring pre-populated with nodes.
    pub fn with_nodes(vnodes_per_node: u32, nodes: impl IntoIterator<Item = N>) -> Self {
        let mut ring = Self::new(vnodes_per_node);
        for node in nodes {
            ring.add_node(node);
        }
        ring
    }

    /// Number of physical nodes on the ring.
    pub fn node_count(&self) -> usize {
        self.node_positions.len()
    }

    /// Number of virtual nodes on the ring.
    pub fn vnode_count(&self) -> usize {
        self.ring.len()
    }

    /// Returns true if the ring has no nodes.
    pub fn is_empty(&self) -> bool {
        self.node_positions.is_empty()
    }

    /// Add a physical node to the ring. If the node already exists, this is a no-op.
    pub fn add_node(&mut self, node: N) {
        if self.node_positions.contains_key(&node) {
            return;
        }

        let mut positions = Vec::with_capacity(self.vnodes as usize);
        for i in 0..self.vnodes {
            let hash = self.vnode_hash(&node, i);
            self.ring.insert(hash, (node.clone(), i));
            positions.push(hash);
        }
        self.node_positions.insert(node, positions);
    }

    /// Remove a physical node from the ring. Returns true if the node was present.
    pub fn remove_node(&mut self, node: &N) -> bool {
        if let Some(positions) = self.node_positions.remove(node) {
            for pos in positions {
                self.ring.remove(&pos);
            }
            true
        } else {
            false
        }
    }

    /// Returns true if the given physical node is on the ring.
    pub fn contains_node(&self, node: &N) -> bool {
        self.node_positions.contains_key(node)
    }

    /// Get the node responsible for the given key.
    /// Returns `None` if the ring is empty.
    pub fn get_node<K: AsRef<[u8]>>(&self, key: K) -> Option<&N> {
        if self.ring.is_empty() {
            return None;
        }
        let hash = fnv1a_hash(key.as_ref());
        // Find the first virtual node at or after this hash position
        if let Some((_, (node, _))) = self.ring.range(hash..).next() {
            Some(node)
        } else {
            // Wrap around to the first node on the ring
            self.ring.values().next().map(|(node, _)| node)
        }
    }

    /// Get up to `count` distinct nodes responsible for the given key.
    /// The first node is the primary; subsequent nodes can serve as replicas.
    /// Returns fewer than `count` if there aren't enough distinct nodes.
    pub fn get_nodes<K: AsRef<[u8]>>(&self, key: K, count: usize) -> Vec<&N> {
        if self.ring.is_empty() || count == 0 {
            return Vec::new();
        }

        let hash = fnv1a_hash(key.as_ref());
        let max = count.min(self.node_positions.len());
        let mut result = Vec::with_capacity(max);
        let mut seen = std::collections::HashSet::new();

        // Walk from hash position forward (with wraparound)
        for (_, (node, _)) in self.ring.range(hash..).chain(self.ring.iter()) {
            if seen.insert(node) {
                result.push(node);
                if result.len() >= max {
                    break;
                }
            }
        }

        result
    }

    /// Get the node for a key, along with the next distinct node on the ring.
    /// Useful for primary + backup assignment.
    pub fn get_node_pair<K: AsRef<[u8]>>(&self, key: K) -> Option<(&N, Option<&N>)> {
        let nodes = self.get_nodes(key, 2);
        match nodes.len() {
            0 => None,
            1 => Some((nodes[0], None)),
            _ => Some((nodes[0], Some(nodes[1]))),
        }
    }

    /// Iterate over all physical nodes.
    pub fn nodes(&self) -> impl Iterator<Item = &N> {
        self.node_positions.keys()
    }

    /// Compute distribution statistics by simulating 10000 key lookups.
    pub fn stats(&self) -> RingStats {
        let node_count = self.node_positions.len();
        let (stddev, min_frac, max_frac) = if node_count == 0 {
            (0.0, 0.0, 0.0)
        } else {
            self.compute_distribution(10000)
        };

        RingStats {
            node_count,
            vnode_count: self.ring.len(),
            vnodes_per_node: self.vnodes,
            distribution_stddev: stddev,
            min_fraction: min_frac,
            max_fraction: max_frac,
        }
    }

    // --- Internal helpers ---

    /// Hash a virtual node position. We combine the node identity with the
    /// virtual index to get well-distributed positions.
    #[allow(clippy::unused_self)]
    fn vnode_hash(&self, node: &N, vnode_idx: u32) -> u64 {
        // Hash the node identity, then mix with the vnode index using
        // multiplicative hashing for better dispersion across the ring.
        let repr = format!("{:?}", node);
        let node_hash = fnv1a_hash(repr.as_bytes());
        // Golden-ratio mixing produces well-spread vnode positions
        let mixed = node_hash.wrapping_add((vnode_idx as u64).wrapping_mul(0x9e3779b97f4a7c15));
        fnv1a_hash(&mixed.to_le_bytes())
    }

    /// Compute distribution statistics by looking up sample keys.
    fn compute_distribution(&self, sample_count: u64) -> (f64, f64, f64) {
        let mut counts: HashMap<&N, u64> = HashMap::new();

        for i in 0..sample_count {
            let key = format!("sample-key-{}", i);
            if let Some(node) = self.get_node(&key) {
                *counts.entry(node).or_insert(0) += 1;
            }
        }

        if counts.is_empty() {
            return (0.0, 0.0, 0.0);
        }

        let n = self.node_positions.len() as f64;
        let expected = sample_count as f64 / n;
        let variance: f64 = counts
            .values()
            .map(|&c| {
                let diff = c as f64 - expected;
                diff * diff
            })
            .sum::<f64>()
            / n;

        let stddev = variance.sqrt() / expected; // Normalized by expected

        let min_count = *counts.values().min().unwrap_or(&0) as f64;
        let max_count = *counts.values().max().unwrap_or(&0) as f64;

        (
            stddev,
            min_count / sample_count as f64,
            max_count / sample_count as f64,
        )
    }
}

/// FNV-1a 64-bit hash (same algorithm used in bloom_filter.rs).
fn fnv1a_hash(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_ring_returns_none() {
        let ring: HashRing<&str> = HashRing::new(100);
        assert!(ring.is_empty());
        assert_eq!(ring.get_node("key"), None);
    }

    #[test]
    fn single_node_always_returns_it() {
        let mut ring = HashRing::new(100);
        ring.add_node("node-0");

        assert_eq!(ring.get_node("any-key"), Some(&"node-0"));
        assert_eq!(ring.get_node("another-key"), Some(&"node-0"));
        assert_eq!(ring.node_count(), 1);
    }

    #[test]
    fn add_and_remove_node() {
        let mut ring = HashRing::new(100);
        ring.add_node("A");
        ring.add_node("B");
        assert_eq!(ring.node_count(), 2);
        assert_eq!(ring.vnode_count(), 200);

        assert!(ring.remove_node(&"A"));
        assert_eq!(ring.node_count(), 1);
        assert_eq!(ring.vnode_count(), 100);

        // All keys should now go to B
        assert_eq!(ring.get_node("test"), Some(&"B"));
    }

    #[test]
    fn remove_nonexistent_node() {
        let mut ring: HashRing<&str> = HashRing::new(100);
        ring.add_node("A");
        assert!(!ring.remove_node(&"Z"));
    }

    #[test]
    fn add_duplicate_node_is_noop() {
        let mut ring = HashRing::new(100);
        ring.add_node("A");
        ring.add_node("A");
        assert_eq!(ring.node_count(), 1);
        assert_eq!(ring.vnode_count(), 100);
    }

    #[test]
    fn contains_node() {
        let mut ring = HashRing::new(100);
        ring.add_node("A");
        assert!(ring.contains_node(&"A"));
        assert!(!ring.contains_node(&"B"));
    }

    #[test]
    fn consistent_key_mapping() {
        let mut ring = HashRing::new(150);
        ring.add_node("A");
        ring.add_node("B");
        ring.add_node("C");

        // Same key should always map to the same node
        let node1 = *ring.get_node("pane-42").unwrap();
        let node2 = *ring.get_node("pane-42").unwrap();
        assert_eq!(node1, node2);
    }

    #[test]
    fn keys_distribute_across_nodes() {
        let mut ring = HashRing::new(150);
        ring.add_node("A");
        ring.add_node("B");
        ring.add_node("C");

        let mut counts: HashMap<&str, usize> = HashMap::new();
        for i in 0..3000 {
            let key = format!("key-{}", i);
            let node = ring.get_node(&key).unwrap();
            *counts.entry(node).or_insert(0) += 1;
        }

        // Each node should get roughly 1000 keys (±300 tolerance)
        for (node, count) in &counts {
            assert!(
                *count > 700 && *count < 1300,
                "Node {} got {} keys, expected ~1000",
                node,
                count
            );
        }
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn minimal_remapping_on_add() {
        let mut ring = HashRing::new(150);
        ring.add_node("A");
        ring.add_node("B");

        // Record key→node mappings
        let keys: Vec<String> = (0..1000).map(|i| format!("key-{}", i)).collect();
        let before: Vec<&str> = keys.iter().map(|k| *ring.get_node(k).unwrap()).collect();

        // Add a third node
        ring.add_node("C");
        let after: Vec<&str> = keys.iter().map(|k| *ring.get_node(k).unwrap()).collect();

        // Count how many keys changed assignment
        let changed: usize = before
            .iter()
            .zip(after.iter())
            .filter(|(b, a)| b != a)
            .count();

        // Adding 1 of 3 nodes should remap ~1/3 of keys (±15% tolerance)
        let expected_fraction = 1.0 / 3.0;
        let actual_fraction = changed as f64 / 1000.0;
        assert!(
            (actual_fraction - expected_fraction).abs() < 0.15,
            "Remapped {:.1}% of keys, expected ~{:.1}%",
            actual_fraction * 100.0,
            expected_fraction * 100.0
        );
    }

    #[test]
    fn minimal_remapping_on_remove() {
        let mut ring = HashRing::new(150);
        ring.add_node("A");
        ring.add_node("B");
        ring.add_node("C");

        let keys: Vec<String> = (0..1000).map(|i| format!("key-{}", i)).collect();
        let before: Vec<&str> = keys.iter().map(|k| *ring.get_node(k).unwrap()).collect();

        ring.remove_node(&"B");
        let after: Vec<&str> = keys.iter().map(|k| *ring.get_node(k).unwrap()).collect();

        // Keys that were on B must move; keys on A/C should stay
        let changed: usize = before
            .iter()
            .zip(after.iter())
            .filter(|(b, a)| b != a)
            .count();

        let was_on_b: usize = before.iter().filter(|&&n| n == "B").count();

        // Only keys from B should move (roughly)
        assert!(
            changed <= was_on_b + 20,
            "Too many keys remapped: changed={}, was_on_B={}",
            changed,
            was_on_b
        );
    }

    #[test]
    fn get_nodes_returns_distinct() {
        let mut ring = HashRing::new(150);
        ring.add_node("A");
        ring.add_node("B");
        ring.add_node("C");

        let nodes = ring.get_nodes("test-key", 3);
        assert_eq!(nodes.len(), 3);

        // All distinct
        let unique: std::collections::HashSet<_> = nodes.iter().collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn get_nodes_capped_by_node_count() {
        let mut ring = HashRing::new(100);
        ring.add_node("A");
        ring.add_node("B");

        let nodes = ring.get_nodes("key", 5); // request 5, only 2 exist
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn get_nodes_empty_ring() {
        let ring: HashRing<&str> = HashRing::new(100);
        let nodes = ring.get_nodes("key", 3);
        assert!(nodes.is_empty());
    }

    #[test]
    fn get_nodes_zero_count() {
        let mut ring = HashRing::new(100);
        ring.add_node("A");
        let nodes = ring.get_nodes("key", 0);
        assert!(nodes.is_empty());
    }

    #[test]
    fn get_node_pair() {
        let mut ring = HashRing::new(150);
        ring.add_node("A");
        ring.add_node("B");

        let pair = ring.get_node_pair("key").unwrap();
        assert!(pair.1.is_some());
        assert_ne!(pair.0, pair.1.unwrap());
    }

    #[test]
    fn get_node_pair_single_node() {
        let mut ring = HashRing::new(100);
        ring.add_node("only");

        let pair = ring.get_node_pair("key").unwrap();
        assert_eq!(pair.0, &"only");
        assert!(pair.1.is_none());
    }

    #[test]
    fn with_nodes_constructor() {
        let ring = HashRing::with_nodes(100, vec!["A", "B", "C"]);
        assert_eq!(ring.node_count(), 3);
        assert_eq!(ring.vnode_count(), 300);
        assert!(ring.get_node("key").is_some());
    }

    #[test]
    fn nodes_iterator() {
        let ring = HashRing::with_nodes(50, vec!["X", "Y", "Z"]);
        let mut nodes: Vec<&&str> = ring.nodes().collect();
        nodes.sort();
        assert_eq!(nodes, vec![&"X", &"Y", &"Z"]);
    }

    #[test]
    fn string_nodes() {
        let mut ring = HashRing::new(100);
        ring.add_node("worker-0".to_string());
        ring.add_node("worker-1".to_string());

        let node = ring.get_node("pane-99").unwrap();
        assert!(node == "worker-0" || node == "worker-1");
    }

    #[test]
    fn integer_nodes() {
        let mut ring = HashRing::new(100);
        ring.add_node(0u32);
        ring.add_node(1u32);
        ring.add_node(2u32);

        let node = ring.get_node("key").unwrap();
        assert!(*node <= 2);
    }

    #[test]
    fn stats_computation() {
        let ring = HashRing::with_nodes(150, vec!["A", "B", "C"]);
        let stats = ring.stats();

        assert_eq!(stats.node_count, 3);
        assert_eq!(stats.vnode_count, 450);
        assert_eq!(stats.vnodes_per_node, 150);
        // Normalized stddev should be reasonable with 150 vnodes
        assert!(
            stats.distribution_stddev < 0.2,
            "Stddev too high: {}",
            stats.distribution_stddev
        );
        // Each node should get 20-45% of keys
        assert!(stats.min_fraction > 0.20, "min={}", stats.min_fraction);
        assert!(stats.max_fraction < 0.45, "max={}", stats.max_fraction);
    }

    #[test]
    fn stats_empty_ring() {
        let ring: HashRing<&str> = HashRing::new(100);
        let stats = ring.stats();
        assert_eq!(stats.node_count, 0);
        assert!(stats.distribution_stddev.abs() < f64::EPSILON);
    }

    #[test]
    fn high_vnode_count_improves_distribution() {
        let ring_low = HashRing::with_nodes(10, vec!["A", "B", "C", "D"]);
        let ring_high = HashRing::with_nodes(500, vec!["A", "B", "C", "D"]);

        let stats_low = ring_low.stats();
        let stats_high = ring_high.stats();

        assert!(
            stats_high.distribution_stddev <= stats_low.distribution_stddev + 0.01,
            "Higher vnodes should give better distribution: low={}, high={}",
            stats_low.distribution_stddev,
            stats_high.distribution_stddev
        );
    }

    #[test]
    fn stress_many_nodes() {
        let nodes: Vec<String> = (0..100).map(|i| format!("node-{}", i)).collect();
        let ring = HashRing::with_nodes(50, nodes.clone());

        assert_eq!(ring.node_count(), 100);
        assert_eq!(ring.vnode_count(), 5000);

        // All 1000 keys should resolve
        for i in 0..1000 {
            assert!(ring.get_node(format!("key-{}", i)).is_some());
        }
    }

    #[test]
    fn stress_add_remove_cycle() {
        let mut ring = HashRing::new(100);
        for round in 0..10 {
            let node = format!("node-{}", round);
            ring.add_node(node.clone());

            if round > 2 {
                let old = format!("node-{}", round - 3);
                ring.remove_node(&old);
            }
        }
        // Should have nodes 7, 8, 9
        assert_eq!(ring.node_count(), 3);
        assert!(ring.get_node("key").is_some());
    }

    #[test]
    #[should_panic(expected = "vnodes_per_node must be > 0")]
    fn zero_vnodes_panics() {
        let _ring: HashRing<&str> = HashRing::new(0);
    }

    #[test]
    fn fnv1a_hash_deterministic() {
        let h1 = fnv1a_hash(b"hello");
        let h2 = fnv1a_hash(b"hello");
        assert_eq!(h1, h2);

        let h3 = fnv1a_hash(b"world");
        assert_ne!(h1, h3);
    }

    #[test]
    fn wraparound_behavior() {
        // With only 1 vnode per node, test that the ring wraps around
        let mut ring = HashRing::new(1);
        ring.add_node("A");
        ring.add_node("B");

        // All keys should resolve (tests wraparound logic)
        for i in 0..100 {
            assert!(ring.get_node(format!("k{}", i)).is_some());
        }
    }

    #[test]
    fn get_nodes_replica_distribution() {
        let ring = HashRing::with_nodes(150, vec!["A", "B", "C", "D", "E"]);

        // For many keys, the first 3 replicas should span at least 3 distinct nodes
        for i in 0..100 {
            let replicas = ring.get_nodes(format!("key-{}", i), 3);
            assert_eq!(replicas.len(), 3);

            let unique: std::collections::HashSet<_> = replicas.iter().collect();
            assert_eq!(unique.len(), 3, "Replicas should be distinct for key-{}", i);
        }
    }
}
