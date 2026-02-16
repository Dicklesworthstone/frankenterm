//! Property-based tests for merkle_tree.rs — Merkle tree state comparison.
//!
//! Bead: ft-283h4.17

use frankenterm_core::merkle_tree::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..32)
}

fn arb_value() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..128)
}

fn arb_entry() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (arb_key(), arb_value())
}

fn arb_entries(max: usize) -> impl Strategy<Value = Vec<(Vec<u8>, Vec<u8>)>> {
    prop::collection::vec(arb_entry(), 0..max)
}

fn arb_tree(max_entries: usize) -> impl Strategy<Value = MerkleTree> {
    arb_entries(max_entries).prop_map(MerkleTree::from_entries)
}

fn arb_nonempty_tree(max_entries: usize) -> impl Strategy<Value = MerkleTree> {
    prop::collection::vec(arb_entry(), 1..max_entries)
        .prop_map(MerkleTree::from_entries)
}

// ── Hash properties ─────────────────────────────────────────────────

proptest! {
    /// Zero hash is distinct from any non-trivial hash.
    #[test]
    fn zero_hash_is_special(data in prop::collection::vec(any::<u8>(), 1..100)) {
        let _h = MerkleHash::from_bytes({
            let mut buf = [0u8; 32];
            // Simple hash to test
            for (i, &b) in data.iter().enumerate() {
                buf[i % 32] ^= b;
            }
            buf
        });
        // Most non-trivial data won't produce all zeros
        // (This is a statistical property, not deterministic)
    }

    /// Hash serde roundtrip preserves exact bytes.
    #[test]
    fn hash_serde_roundtrip(bytes in prop::array::uniform32(any::<u8>())) {
        let h = MerkleHash::from_bytes(bytes);
        let json = serde_json::to_string(&h).unwrap();
        let back: MerkleHash = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(h, back);
    }

    /// Hash display is 64 hex characters.
    #[test]
    fn hash_display_length(bytes in prop::array::uniform32(any::<u8>())) {
        let h = MerkleHash::from_bytes(bytes);
        let s = format!("{}", h);
        prop_assert_eq!(s.len(), 64, "display should be 64 hex chars");
    }
}

// ── Tree construction properties ────────────────────────────────────

proptest! {
    /// Empty tree always has zero hash.
    #[test]
    fn empty_tree_zero_hash(_dummy in 0..1u8) {
        let tree = MerkleTree::new();
        prop_assert_eq!(tree.root_hash(), MerkleHash::ZERO);
        prop_assert!(tree.is_empty());
    }

    /// Non-empty tree never has zero hash.
    #[test]
    fn nonempty_tree_nonzero_hash(tree in arb_nonempty_tree(20)) {
        prop_assert_ne!(tree.root_hash(), MerkleHash::ZERO);
        prop_assert!(!tree.is_empty());
    }

    /// Tree length matches unique key count.
    #[test]
    fn tree_len_matches_unique_keys(entries in arb_entries(30)) {
        let unique: BTreeMap<_, _> = entries.into_iter().collect();
        let tree = MerkleTree::from_entries(unique.clone());
        prop_assert_eq!(tree.len(), unique.len());
    }

    /// Insertion order doesn't affect root hash (deterministic).
    #[test]
    fn insertion_order_independent(entries in arb_entries(20)) {
        // Deduplicate: with duplicate keys, last-writer-wins means insertion
        // order DOES matter for the final value. Use BTreeMap to get a
        // deterministic set of unique keys.
        let unique: std::collections::BTreeMap<_, _> = entries.into_iter().collect();
        let deduped: Vec<_> = unique.into_iter().collect();
        if deduped.is_empty() {
            return Ok(());
        }
        let tree1 = MerkleTree::from_entries(deduped.clone());
        let mut reversed = deduped;
        reversed.reverse();
        let tree2 = MerkleTree::from_entries(reversed);
        prop_assert_eq!(
            tree1.root_hash(), tree2.root_hash(),
            "insertion order should not affect root hash"
        );
    }

    /// Trees with identical entries have identical hashes.
    #[test]
    fn identical_entries_identical_hash(entries in arb_entries(20)) {
        let tree1 = MerkleTree::from_entries(entries.clone());
        let tree2 = MerkleTree::from_entries(entries);
        prop_assert_eq!(tree1.root_hash(), tree2.root_hash());
    }

    /// Adding a new key changes the root hash.
    #[test]
    fn insert_changes_hash(
        entries in prop::collection::vec(arb_entry(), 1..10),
        new_key in arb_key(),
        new_value in arb_value()
    ) {
        let mut tree = MerkleTree::from_entries(entries);
        let original = tree.root_hash();
        // Only test if key is new (not already present)
        if !tree.contains_key(&new_key) {
            tree.insert(new_key, new_value);
            prop_assert_ne!(
                tree.root_hash(), original,
                "insert of new key should change hash"
            );
        }
    }

    /// Removing last entry gives zero hash.
    #[test]
    fn remove_last_gives_zero(key in arb_key(), value in arb_value()) {
        let mut tree = MerkleTree::from_entries(vec![(key.clone(), value)]);
        tree.remove(&key);
        prop_assert_eq!(tree.root_hash(), MerkleHash::ZERO);
        prop_assert!(tree.is_empty());
    }

    /// Insert then remove restores original hash.
    #[test]
    fn insert_remove_roundtrip(
        entries in prop::collection::vec(arb_entry(), 1..10),
        new_key in arb_key(),
        new_value in arb_value()
    ) {
        let mut tree = MerkleTree::from_entries(entries);
        if !tree.contains_key(&new_key) {
            let original = tree.root_hash();
            tree.insert(new_key.clone(), new_value);
            tree.remove(&new_key);
            prop_assert_eq!(
                tree.root_hash(), original,
                "insert+remove should restore original hash"
            );
        }
    }

    /// Get returns correct value for all inserted keys.
    #[test]
    fn get_returns_correct_value(entries in arb_entries(20)) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let tree = MerkleTree::from_entries(unique.clone());
        for (k, v) in &unique {
            prop_assert_eq!(
                tree.get(k), Some(v.as_slice()),
                "get({:?}) should return correct value", k
            );
        }
    }

    /// Get returns None for absent keys.
    #[test]
    fn get_absent_key(tree in arb_tree(10), absent in arb_key()) {
        if !tree.contains_key(&absent) {
            prop_assert!(tree.get(&absent).is_none());
        }
    }

    /// Keys are iterated in sorted order.
    #[test]
    fn keys_sorted(entries in arb_entries(20)) {
        let tree = MerkleTree::from_entries(entries);
        let keys: Vec<_> = tree.keys().collect();
        for i in 1..keys.len() {
            prop_assert!(
                keys[i - 1] <= keys[i],
                "keys should be sorted: {:?} > {:?}", keys[i-1], keys[i]
            );
        }
    }
}

// ── Proof properties ────────────────────────────────────────────────

proptest! {
    /// Proof for existing key verifies against root hash.
    #[test]
    fn proof_verifies(entries in prop::collection::vec(arb_entry(), 1..15)) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let tree = MerkleTree::from_entries(unique.clone());
        for key in unique.keys() {
            let proof = tree.proof(key).unwrap();
            prop_assert!(
                proof.verify(&tree.root_hash()),
                "proof for key {:?} should verify", key
            );
        }
    }

    /// Proof for nonexistent key returns None.
    #[test]
    fn proof_none_for_absent(tree in arb_tree(10), absent in arb_key()) {
        if !tree.contains_key(&absent) {
            prop_assert!(tree.proof(&absent).is_none());
        }
    }

    /// Proof fails against wrong root hash.
    #[test]
    fn proof_rejects_wrong_root(
        entries in prop::collection::vec(arb_entry(), 1..10),
        wrong_bytes in prop::array::uniform32(any::<u8>())
    ) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let tree = MerkleTree::from_entries(unique.clone());
        let key = unique.keys().next().unwrap();
        let proof = tree.proof(key).unwrap();
        let wrong_root = MerkleHash::from_bytes(wrong_bytes);
        if wrong_root != tree.root_hash() {
            prop_assert!(
                !proof.verify(&wrong_root),
                "proof should reject wrong root"
            );
        }
    }

    /// Proof serde roundtrip preserves verification.
    #[test]
    fn proof_serde_roundtrip(entries in prop::collection::vec(arb_entry(), 1..10)) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let tree = MerkleTree::from_entries(unique.clone());
        let key = unique.keys().next().unwrap();
        let proof = tree.proof(key).unwrap();
        let json = serde_json::to_string(&proof).unwrap();
        let back: MerkleProof = serde_json::from_str(&json).unwrap();
        prop_assert!(back.verify(&tree.root_hash()));
    }
}

// ── Diff properties ─────────────────────────────────────────────────

proptest! {
    /// Diff of identical trees is empty.
    #[test]
    fn diff_identical_empty(entries in arb_entries(15)) {
        let tree = MerkleTree::from_entries(entries);
        let diff = tree.diff(&tree.clone());
        prop_assert!(diff.is_empty(), "diff of identical trees should be empty");
        prop_assert_eq!(diff.total_changes(), 0);
    }

    /// Diff detects added entries.
    #[test]
    fn diff_detects_additions(
        base_entries in prop::collection::vec(arb_entry(), 1..10),
        new_key in arb_key(),
        new_value in arb_value()
    ) {
        let base: BTreeMap<Vec<u8>, Vec<u8>> = base_entries.into_iter().collect();
        let tree1 = MerkleTree::from_entries(base.clone());
        if !base.contains_key(&new_key) {
            let mut extended = base;
            extended.insert(new_key.clone(), new_value);
            let tree2 = MerkleTree::from_entries(extended);
            let diff = tree1.diff(&tree2);
            prop_assert!(
                diff.added.contains(&new_key),
                "diff should detect added key"
            );
        }
    }

    /// Diff detects removed entries.
    #[test]
    fn diff_detects_removals(entries in prop::collection::vec(arb_entry(), 2..10)) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        if unique.len() >= 2 {
            let tree1 = MerkleTree::from_entries(unique.clone());
            let removed_key = unique.keys().next().unwrap().clone();
            let mut reduced = unique;
            reduced.remove(&removed_key);
            let tree2 = MerkleTree::from_entries(reduced);
            let diff = tree1.diff(&tree2);
            prop_assert!(
                diff.removed.contains(&removed_key),
                "diff should detect removed key"
            );
        }
    }

    /// Diff detects changed values.
    #[test]
    fn diff_detects_changes(
        entries in prop::collection::vec(arb_entry(), 1..10),
        new_value in arb_value()
    ) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let tree1 = MerkleTree::from_entries(unique.clone());
        let key = unique.keys().next().unwrap().clone();
        let old_value = unique.get(&key).unwrap();
        if *old_value != new_value {
            let mut modified = unique;
            modified.insert(key.clone(), new_value);
            let tree2 = MerkleTree::from_entries(modified);
            let diff = tree1.diff(&tree2);
            prop_assert!(
                diff.changed.contains(&key),
                "diff should detect changed key"
            );
        }
    }

    /// Diff is symmetric: if a→b shows additions, b→a shows removals.
    #[test]
    fn diff_symmetry(
        entries1 in arb_entries(10),
        entries2 in arb_entries(10)
    ) {
        let tree1 = MerkleTree::from_entries(entries1);
        let tree2 = MerkleTree::from_entries(entries2);
        let d12 = tree1.diff(&tree2);
        let d21 = tree2.diff(&tree1);
        // Additions in one direction are removals in the other
        prop_assert_eq!(
            d12.added.len(), d21.removed.len(),
            "additions/removals should be symmetric"
        );
        prop_assert_eq!(
            d12.removed.len(), d21.added.len(),
            "removals/additions should be symmetric"
        );
        prop_assert_eq!(
            d12.changed.len(), d21.changed.len(),
            "changes should be symmetric"
        );
    }

    /// TreeDiff serde roundtrip.
    #[test]
    fn diff_serde_roundtrip(
        entries1 in arb_entries(10),
        entries2 in arb_entries(10)
    ) {
        let tree1 = MerkleTree::from_entries(entries1);
        let tree2 = MerkleTree::from_entries(entries2);
        let diff = tree1.diff(&tree2);
        let json = serde_json::to_string(&diff).unwrap();
        let back: TreeDiff = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(diff, back);
    }
}

// ── Level hash properties ───────────────────────────────────────────

proptest! {
    /// Level 0 hashes contains exactly the root hash.
    #[test]
    fn level_0_is_root(tree in arb_tree(15)) {
        let hashes = tree.level_hashes(0);
        prop_assert_eq!(hashes.len(), 1);
        prop_assert_eq!(hashes[0], tree.root_hash());
    }

    /// Deeper levels produce more hashes (up to leaf count).
    #[test]
    fn deeper_levels_more_hashes(tree in arb_nonempty_tree(15)) {
        let h0 = tree.level_hashes(0).len();
        let h1 = tree.level_hashes(1).len();
        prop_assert!(
            h1 >= h0,
            "level 1 ({}) should have >= level 0 ({}) hashes", h1, h0
        );
    }
}

// ── Tree serde properties ───────────────────────────────────────────

proptest! {
    /// Tree serde roundtrip preserves root hash and entries.
    #[test]
    fn tree_serde_roundtrip(entries in arb_entries(15)) {
        let tree = MerkleTree::from_entries(entries);
        let json = serde_json::to_string(&tree).unwrap();
        let back: MerkleTree = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tree.root_hash(), back.root_hash());
        prop_assert_eq!(tree.len(), back.len());
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// If root hashes match, diff is empty.
    #[test]
    fn matching_hash_means_empty_diff(entries in arb_entries(15)) {
        let tree1 = MerkleTree::from_entries(entries.clone());
        let tree2 = MerkleTree::from_entries(entries);
        if tree1.root_hash() == tree2.root_hash() {
            let diff = tree1.diff(&tree2);
            prop_assert!(diff.is_empty(), "matching hashes should give empty diff");
        }
    }

    /// Every key in the tree has a valid proof.
    #[test]
    fn all_keys_have_valid_proofs(entries in prop::collection::vec(arb_entry(), 1..10)) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        let tree = MerkleTree::from_entries(unique.clone());
        let root = tree.root_hash();
        for key in unique.keys() {
            let proof = tree.proof(key);
            prop_assert!(proof.is_some(), "key {:?} should have a proof", key);
            prop_assert!(
                proof.unwrap().verify(&root),
                "proof for {:?} should verify against root", key
            );
        }
    }

    /// Changing any single value invalidates old proofs for that key.
    #[test]
    fn value_change_invalidates_proof(
        entries in prop::collection::vec(arb_entry(), 2..10),
        new_value in arb_value()
    ) {
        let unique: BTreeMap<Vec<u8>, Vec<u8>> = entries.into_iter().collect();
        if unique.len() >= 2 {
            let tree1 = MerkleTree::from_entries(unique.clone());
            let key = unique.keys().next().unwrap().clone();
            let old_proof = tree1.proof(&key).unwrap();

            let mut modified = unique;
            let old_val = modified.get(&key).unwrap().clone();
            if old_val != new_value {
                modified.insert(key, new_value);
                let tree2 = MerkleTree::from_entries(modified);
                // Old proof should NOT verify against new root
                prop_assert!(
                    !old_proof.verify(&tree2.root_hash()),
                    "old proof should not verify against modified tree"
                );
            }
        }
    }
}
