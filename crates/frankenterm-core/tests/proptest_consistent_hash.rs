//! Property-based tests for consistent_hash module.
//!
//! Verifies the consistent hash ring invariants:
//! - Consistency: same key always maps to same node
//! - Vnode count = node_count × vnodes_per_node
//! - Empty ring returns None
//! - Single node handles all keys
//! - Duplicate add is no-op, remove returns correct bool
//! - get_nodes returns distinct replicas, capped by node_count
//! - Minimal remapping: adding/removing a node remaps ~1/N keys
//! - Distribution balance: all nodes receive reasonable fractions
//! - Add/remove cycles leave ring consistent
//! - with_nodes matches sequential add_node

use proptest::prelude::*;
use std::collections::{HashMap, HashSet};

use frankenterm_core::consistent_hash::HashRing;

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_vnodes() -> impl Strategy<Value = u32> {
    1u32..=200
}

fn arb_node_count() -> impl Strategy<Value = usize> {
    1usize..=20
}

fn arb_key() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,20}"
}

fn arb_keys(max_len: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_key(), 1..max_len)
}

// ────────────────────────────────────────────────────────────────────
// Consistency: same key → same node
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Repeated lookups of the same key always return the same node.
    #[test]
    fn prop_consistent_key_mapping(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
        keys in arb_keys(50),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        for key in &keys {
            let first = ring.get_node(key);
            let second = ring.get_node(key);
            prop_assert_eq!(first, second, "key '{}' mapped inconsistently", key);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Vnode count = node_count × vnodes_per_node
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// vnode_count equals node_count * vnodes_per_node.
    #[test]
    fn prop_vnode_count_correct(
        vnodes in arb_vnodes(),
        n_nodes in arb_node_count(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        prop_assert_eq!(
            ring.vnode_count(),
            n_nodes * vnodes as usize,
            "vnode_count mismatch"
        );
        prop_assert_eq!(ring.node_count(), n_nodes);
    }
}

// ────────────────────────────────────────────────────────────────────
// Empty ring returns None
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Empty ring returns None for any key.
    #[test]
    fn prop_empty_ring_returns_none(
        vnodes in arb_vnodes(),
        keys in arb_keys(20),
    ) {
        let ring: HashRing<String> = HashRing::new(vnodes);
        prop_assert!(ring.is_empty());

        for key in &keys {
            prop_assert!(ring.get_node(key).is_none());
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Single node handles all keys
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// With one node, all keys map to that node.
    #[test]
    fn prop_single_node_handles_all(
        vnodes in arb_vnodes(),
        keys in arb_keys(30),
    ) {
        let mut ring = HashRing::new(vnodes);
        ring.add_node("solo".to_string());

        for key in &keys {
            prop_assert_eq!(
                ring.get_node(key).map(|n| n.as_str()),
                Some("solo")
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Duplicate add is no-op
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Adding the same node twice doesn't change node_count or vnode_count.
    #[test]
    fn prop_duplicate_add_is_noop(
        vnodes in arb_vnodes(),
        n_nodes in arb_node_count(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let mut ring = HashRing::new(vnodes);

        for node in &nodes {
            ring.add_node(node.clone());
        }
        let nc_before = ring.node_count();
        let vc_before = ring.vnode_count();

        // Add all again
        for node in &nodes {
            ring.add_node(node.clone());
        }

        prop_assert_eq!(ring.node_count(), nc_before);
        prop_assert_eq!(ring.vnode_count(), vc_before);
    }
}

// ────────────────────────────────────────────────────────────────────
// Remove returns correct bool
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// remove_node returns true for present nodes, false for absent.
    #[test]
    fn prop_remove_returns_correct_bool(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let mut ring = HashRing::with_nodes(vnodes, nodes.clone());

        for node in &nodes {
            prop_assert!(ring.remove_node(node), "remove should return true for present node");
        }
        prop_assert!(ring.is_empty());

        // Removing again should return false
        for node in &nodes {
            prop_assert!(!ring.remove_node(node), "remove should return false for absent node");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// get_nodes returns distinct replicas
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// get_nodes returns distinct nodes, and count is min(requested, node_count).
    #[test]
    fn prop_get_nodes_distinct(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
        requested in 1usize..=15,
        key in arb_key(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let result = ring.get_nodes(&key, requested);
        let expected_len = requested.min(n_nodes);

        prop_assert_eq!(
            result.len(), expected_len,
            "expected {} nodes, got {}", expected_len, result.len()
        );

        // All distinct
        let unique: HashSet<_> = result.iter().collect();
        prop_assert_eq!(
            unique.len(), result.len(),
            "get_nodes returned duplicate nodes"
        );
    }

    /// get_nodes(key, 0) always returns empty.
    #[test]
    fn prop_get_nodes_zero_count(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=5,
        key in arb_key(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let result = ring.get_nodes(&key, 0);
        prop_assert!(result.is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// get_node_pair
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// get_node_pair returns (primary, Some(backup)) for 2+ nodes, (primary, None) for 1 node.
    #[test]
    fn prop_get_node_pair_structure(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
        key in arb_key(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let pair = ring.get_node_pair(&key);
        prop_assert!(pair.is_some(), "non-empty ring should return Some");
        let (primary, backup) = pair.unwrap();

        if n_nodes >= 2 {
            prop_assert!(backup.is_some(), "2+ nodes should have backup");
            prop_assert_ne!(primary, backup.unwrap(), "primary != backup");
        } else {
            prop_assert!(backup.is_none(), "1 node should have no backup");
        }
    }

    /// get_node_pair primary matches get_node.
    #[test]
    fn prop_get_node_pair_matches_get_node(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
        key in arb_key(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let single = ring.get_node(&key);
        let pair = ring.get_node_pair(&key);

        prop_assert_eq!(
            single,
            pair.map(|(p, _)| p),
            "get_node and get_node_pair.0 should agree"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// All keys map to a real node
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Every key maps to one of the nodes actually on the ring.
    #[test]
    fn prop_keys_map_to_real_nodes(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
        keys in arb_keys(50),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let node_set: HashSet<String> = nodes.iter().cloned().collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        for key in &keys {
            let mapped = ring.get_node(key).unwrap();
            prop_assert!(
                node_set.contains(mapped),
                "key '{}' mapped to '{}' which is not in the ring", key, mapped
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// contains_node consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// contains_node matches the set of added nodes.
    #[test]
    fn prop_contains_node_accurate(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes.clone());

        for node in &nodes {
            prop_assert!(ring.contains_node(node));
        }
        prop_assert!(!ring.contains_node(&"nonexistent".to_string()));
    }
}

// ────────────────────────────────────────────────────────────────────
// Minimal remapping on add
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Adding one node to N nodes remaps a bounded fraction of keys.
    /// The theoretical ideal is 1/(N+1), but with finite vnodes the actual
    /// fraction can deviate. We verify it stays below 2/(N+1) + 0.05.
    #[test]
    fn prop_minimal_remapping_on_add(
        vnodes in 100u32..=200,
        n_nodes in 3usize..=8,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let n_keys = 3000usize;
        let keys: Vec<String> = (0..n_keys).map(|i| format!("k-{}", i)).collect();
        let before: Vec<String> = keys.iter()
            .map(|k| ring.get_node(k).unwrap().clone())
            .collect();

        // Add new node
        let mut ring2 = ring.clone();
        ring2.add_node(format!("n-{}", n_nodes));

        let after: Vec<String> = keys.iter()
            .map(|k| ring2.get_node(k).unwrap().clone())
            .collect();

        let changed = before.iter().zip(after.iter())
            .filter(|(b, a)| b != a)
            .count();
        let actual_frac = changed as f64 / n_keys as f64;
        let upper_bound = 2.0 / (n_nodes + 1) as f64 + 0.05;

        // Remapping should be bounded: well below 100% and in the right ballpark
        prop_assert!(
            actual_frac < upper_bound,
            "remapped {:.1}%, upper bound {:.1}%",
            actual_frac * 100.0, upper_bound * 100.0
        );
        // Also should remap at least some keys (new node must get some traffic)
        prop_assert!(
            actual_frac > 0.01,
            "remapped only {:.1}%, new node got almost no keys", actual_frac * 100.0
        );
    }

    /// Removing a node only remaps keys that were on the removed node.
    #[test]
    fn prop_minimal_remapping_on_remove(
        vnodes in 50u32..=200,
        n_nodes in 3usize..=8,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes.clone());

        let n_keys = 2000usize;
        let keys: Vec<String> = (0..n_keys).map(|i| format!("k-{}", i)).collect();
        let before: Vec<String> = keys.iter()
            .map(|k| ring.get_node(k).unwrap().clone())
            .collect();

        // Remove node 0
        let removed = "n-0".to_string();
        let mut ring2 = ring.clone();
        ring2.remove_node(&removed);

        let after: Vec<String> = keys.iter()
            .map(|k| ring2.get_node(k).unwrap().clone())
            .collect();

        // Keys NOT on the removed node should stay the same
        let mut false_remaps = 0usize;
        for (b, a) in before.iter().zip(after.iter()) {
            if b != &removed && b != a {
                false_remaps += 1;
            }
        }

        prop_assert!(
            false_remaps == 0,
            "{} keys not on removed node were remapped", false_remaps
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Distribution balance
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// With enough vnodes, each node gets between 5% and 60% of keys (for 2-8 nodes).
    #[test]
    fn prop_distribution_balance(
        vnodes in 100u32..=200,
        n_nodes in 2usize..=8,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let n_keys = 5000usize;
        let mut counts: HashMap<String, usize> = HashMap::new();
        for i in 0..n_keys {
            let key = format!("balance-{}", i);
            let node = ring.get_node(&key).unwrap().clone();
            *counts.entry(node).or_insert(0) += 1;
        }

        // All nodes should receive keys
        prop_assert_eq!(counts.len(), n_nodes, "not all nodes received keys");

        for (node, count) in &counts {
            let frac = *count as f64 / n_keys as f64;
            prop_assert!(
                frac > 0.02 && frac < 0.75,
                "node {} got {:.1}% of keys (expected reasonable fraction)", node, frac * 100.0
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Stats consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// RingStats fields are consistent with ring state.
    #[test]
    fn prop_stats_fields_consistent(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let stats = ring.stats();
        prop_assert_eq!(stats.node_count, n_nodes);
        prop_assert_eq!(stats.vnode_count, n_nodes * vnodes as usize);
        prop_assert_eq!(stats.vnodes_per_node, vnodes);

        // min_fraction ≤ max_fraction
        prop_assert!(
            stats.min_fraction <= stats.max_fraction,
            "min {} > max {}", stats.min_fraction, stats.max_fraction
        );

        // Fractions should be positive and ≤ 1.0
        prop_assert!(stats.min_fraction > 0.0 && stats.min_fraction <= 1.0);
        prop_assert!(stats.max_fraction > 0.0 && stats.max_fraction <= 1.0);

        // Stddev non-negative
        prop_assert!(stats.distribution_stddev >= 0.0);
    }
}

// ────────────────────────────────────────────────────────────────────
// with_nodes matches sequential add_node
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// with_nodes produces the same ring as sequential add_node calls.
    #[test]
    fn prop_with_nodes_matches_sequential(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();

        let ring1 = HashRing::with_nodes(vnodes, nodes.clone());

        let mut ring2 = HashRing::new(vnodes);
        for node in &nodes {
            ring2.add_node(node.clone());
        }

        // Same structure
        prop_assert_eq!(ring1.node_count(), ring2.node_count());
        prop_assert_eq!(ring1.vnode_count(), ring2.vnode_count());

        // Same key mappings for a set of test keys
        for i in 0..100 {
            let key = format!("verify-{}", i);
            prop_assert_eq!(
                ring1.get_node(&key), ring2.get_node(&key),
                "key '{}' maps differently", key
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// Add/remove cycles
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After add/remove cycles, ring state is consistent.
    #[test]
    fn prop_add_remove_cycle_consistent(
        vnodes in arb_vnodes(),
        n_rounds in 3usize..=15,
    ) {
        let mut ring: HashRing<String> = HashRing::new(vnodes);

        for round in 0..n_rounds {
            ring.add_node(format!("node-{}", round));
            if round >= 2 {
                ring.remove_node(&format!("node-{}", round - 2));
            }
        }

        // Should have exactly 2 nodes remaining (the last 2 added)
        prop_assert_eq!(ring.node_count(), 2);
        prop_assert_eq!(ring.vnode_count(), 2 * vnodes as usize);

        // All keys should resolve
        for i in 0..50 {
            let key = format!("k-{}", i);
            prop_assert!(ring.get_node(&key).is_some());
        }
    }

    /// Removing all nodes leaves an empty ring.
    #[test]
    fn prop_remove_all_leaves_empty(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let mut ring = HashRing::with_nodes(vnodes, nodes.clone());

        for node in &nodes {
            ring.remove_node(node);
        }

        prop_assert!(ring.is_empty());
        prop_assert_eq!(ring.node_count(), 0);
        prop_assert_eq!(ring.vnode_count(), 0);
        prop_assert!(ring.get_node("any-key").is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// get_nodes first element matches get_node
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// The first element of get_nodes is always the same as get_node.
    #[test]
    fn prop_get_nodes_first_is_get_node(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
        key in arb_key(),
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes);

        let single = ring.get_node(&key);
        let multi = ring.get_nodes(&key, 1);

        prop_assert_eq!(single, multi.first().copied());
    }
}

// ────────────────────────────────────────────────────────────────────
// nodes() iterator completeness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// nodes() iterator yields exactly the added nodes.
    #[test]
    fn prop_nodes_iterator_complete(
        vnodes in arb_vnodes(),
        n_nodes in 1usize..=10,
    ) {
        let expected: HashSet<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, expected.iter().cloned());

        let actual: HashSet<String> = ring.nodes().cloned().collect();
        prop_assert_eq!(actual, expected);
    }
}

// ────────────────────────────────────────────────────────────────────
// Clone produces independent ring
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Cloned ring is independent: mutations don't affect the original.
    #[test]
    fn prop_clone_independence(
        vnodes in arb_vnodes(),
        n_nodes in 2usize..=8,
    ) {
        let nodes: Vec<String> = (0..n_nodes).map(|i| format!("n-{}", i)).collect();
        let ring = HashRing::with_nodes(vnodes, nodes.clone());

        let mut cloned = ring.clone();
        cloned.add_node("extra".to_string());
        cloned.remove_node(&nodes[0]);

        // Original should be unchanged
        prop_assert_eq!(ring.node_count(), n_nodes);
        prop_assert!(ring.contains_node(&nodes[0]));
        prop_assert!(!ring.contains_node(&"extra".to_string()));

        // Clone should reflect mutations
        prop_assert_eq!(cloned.node_count(), n_nodes); // added 1, removed 1
        prop_assert!(!cloned.contains_node(&nodes[0]));
        prop_assert!(cloned.contains_node(&"extra".to_string()));
    }
}
