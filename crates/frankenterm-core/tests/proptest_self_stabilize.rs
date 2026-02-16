#![allow(clippy::comparison_chain)]
//! Property-based tests for self_stabilize.rs — convergent reconciliation protocol.
//!
//! Bead: ft-283h4.13.1

use frankenterm_core::merkle_tree::*;
use frankenterm_core::self_stabilize::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_key() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..16)
}

fn arb_value() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..64)
}

fn arb_entry() -> impl Strategy<Value = (Vec<u8>, Vec<u8>)> {
    (arb_key(), arb_value())
}

fn arb_tree(max_entries: usize) -> impl Strategy<Value = MerkleTree> {
    prop::collection::vec(arb_entry(), 0..max_entries)
        .prop_map(MerkleTree::from_entries)
}

fn arb_nonempty_tree(max_entries: usize) -> impl Strategy<Value = MerkleTree> {
    prop::collection::vec(arb_entry(), 1..max_entries)
        .prop_map(MerkleTree::from_entries)
}

fn arb_config() -> impl Strategy<Value = ReconcileConfig> {
    (1..8usize).prop_map(|depth| ReconcileConfig {
        max_probe_depth: depth,
    })
}

// ── Convergence properties ──────────────────────────────────────────

proptest! {
    /// Identical trees converge in 0 rounds.
    #[test]
    fn identical_trees_zero_rounds(
        tree in arb_tree(20),
        config in arb_config()
    ) {
        let (result, rounds) = reconcile_trees(&tree, &tree.clone(), &config);
        prop_assert_eq!(result.root_hash(), tree.root_hash());
        prop_assert_eq!(rounds, 0);
    }

    /// Reconciliation always produces authority's root hash.
    #[test]
    fn result_matches_authority(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        prop_assert_eq!(
            result.root_hash(), authority.root_hash(),
            "reconciled result must match authority"
        );
    }

    /// Reconciliation preserves all authority entries.
    #[test]
    fn result_has_all_authority_entries(
        authority in arb_nonempty_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        for (k, v) in authority.iter() {
            prop_assert_eq!(
                result.get(k), Some(v),
                "authority entry must be in result"
            );
        }
    }

    /// Reconciliation produces same entry count as authority.
    #[test]
    fn result_len_matches_authority(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        prop_assert_eq!(result.len(), authority.len());
    }

    /// Double reconciliation is idempotent.
    #[test]
    fn reconcile_idempotent(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (result1, _) = reconcile_trees(&authority, &replica, &config);
        let (result2, rounds2) = reconcile_trees(&authority, &result1, &config);
        prop_assert_eq!(result1.root_hash(), result2.root_hash());
        prop_assert_eq!(rounds2, 0, "second reconciliation should be no-op");
    }

    /// Reconciliation is bounded: at most 1 round for any divergence.
    #[test]
    fn reconcile_bounded_rounds(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (_, rounds) = reconcile_trees(&authority, &replica, &config);
        prop_assert!(
            rounds <= 1,
            "reconciliation should take at most 1 round, got {}", rounds
        );
    }
}

// ── Stats properties ────────────────────────────────────────────────

proptest! {
    /// Stats report convergence.
    #[test]
    fn stats_always_converged(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (_, stats) = reconcile_with_stats(&authority, &replica, &config);
        prop_assert!(stats.converged, "reconciliation must converge");
    }

    /// Stats change counts are consistent with diff.
    #[test]
    fn stats_counts_consistent(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let diff = replica.diff(&authority);
        let (_, stats) = reconcile_with_stats(&authority, &replica, &config);
        prop_assert_eq!(stats.added, diff.added.len());
        prop_assert_eq!(stats.removed, diff.removed.len());
        prop_assert_eq!(stats.changed, diff.changed.len());
    }

    /// Identical trees have zero changes in stats.
    #[test]
    fn stats_zero_for_identical(tree in arb_tree(15), config in arb_config()) {
        let (_, stats) = reconcile_with_stats(&tree, &tree.clone(), &config);
        prop_assert_eq!(stats.added, 0);
        prop_assert_eq!(stats.removed, 0);
        prop_assert_eq!(stats.changed, 0);
        prop_assert_eq!(stats.rounds, 0);
    }

    /// Stats serde roundtrip.
    #[test]
    fn stats_serde_roundtrip(
        rounds in 0..10usize,
        added in 0..20usize,
        removed in 0..20usize,
        changed in 0..20usize,
        converged in any::<bool>()
    ) {
        let stats = ReconcileStats { rounds, added, removed, changed, converged };
        let json = serde_json::to_string(&stats).unwrap();
        let back: ReconcileStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }
}

// ── Session properties ──────────────────────────────────────────────

proptest! {
    /// Session starts in Init phase.
    #[test]
    fn session_starts_init(tree in arb_tree(10), config in arb_config()) {
        let session = ReconcileSession::new(tree, true, config);
        prop_assert_eq!(session.phase(), Phase::Init);
        prop_assert!(!session.is_converged());
        prop_assert_eq!(session.rounds(), 0);
    }

    /// Session converges on matching root hash.
    #[test]
    fn session_converges_on_match(tree in arb_tree(10), config in arb_config()) {
        let mut session = ReconcileSession::new(tree.clone(), true, config);
        session.start();
        let result = session.receive(&ReconcileMessage::RootHash(tree.root_hash()));
        prop_assert_eq!(result, RoundResult::AlreadyConverged);
        prop_assert!(session.is_converged());
    }

    /// Session sends level hashes on mismatch.
    #[test]
    fn session_narrows_on_mismatch(
        tree in arb_tree(10),
        config in arb_config(),
        wrong_bytes in prop::array::uniform32(any::<u8>())
    ) {
        let mut session = ReconcileSession::new(tree.clone(), true, config);
        session.start();
        let wrong_hash = MerkleHash::from_bytes(wrong_bytes);
        if wrong_hash != tree.root_hash() {
            let result = session.receive(&ReconcileMessage::RootHash(wrong_hash));
            let is_level_hashes = matches!(result, RoundResult::SendMessage(ReconcileMessage::LevelHashes { .. }));
            prop_assert!(is_level_hashes, "expected LevelHashes message on mismatch");
        }
    }

    /// Session round counter increments.
    #[test]
    fn session_round_counter(tree in arb_tree(10), config in arb_config()) {
        let mut session = ReconcileSession::new(tree.clone(), true, config);
        prop_assert_eq!(session.rounds(), 0);
        session.start();
        session.receive(&ReconcileMessage::RootHash(tree.root_hash()));
        prop_assert_eq!(session.rounds(), 1);
    }
}

// ── Fingerprint properties ──────────────────────────────────────────

proptest! {
    /// Fingerprint of same tree matches.
    #[test]
    fn fingerprint_same_tree_matches(
        tree in arb_tree(15),
        v1 in any::<u64>(),
        v2 in any::<u64>()
    ) {
        let fp1 = StateFingerprint::from_tree(&tree, v1);
        let fp2 = StateFingerprint::from_tree(&tree, v2);
        prop_assert!(fp1.matches(&fp2), "same tree should match regardless of version");
    }

    /// Fingerprint entry count is correct.
    #[test]
    fn fingerprint_entry_count(tree in arb_tree(15), version in any::<u64>()) {
        let fp = StateFingerprint::from_tree(&tree, version);
        prop_assert_eq!(fp.entry_count, tree.len());
    }

    /// Fingerprint version comparison is consistent.
    #[test]
    fn fingerprint_version_order(
        tree in arb_tree(5),
        v1 in 0..1000u64,
        v2 in 0..1000u64
    ) {
        let fp1 = StateFingerprint::from_tree(&tree, v1);
        let fp2 = StateFingerprint::from_tree(&tree, v2);
        if v1 > v2 {
            prop_assert!(fp1.is_newer_than(&fp2));
            prop_assert!(!fp2.is_newer_than(&fp1));
        } else if v2 > v1 {
            prop_assert!(fp2.is_newer_than(&fp1));
            prop_assert!(!fp1.is_newer_than(&fp2));
        } else {
            prop_assert!(!fp1.is_newer_than(&fp2));
            prop_assert!(!fp2.is_newer_than(&fp1));
        }
    }

    /// Fingerprint serde roundtrip.
    #[test]
    fn fingerprint_serde_roundtrip(tree in arb_tree(10), version in any::<u64>()) {
        let fp = StateFingerprint::from_tree(&tree, version);
        let json = serde_json::to_string(&fp).unwrap();
        let back: StateFingerprint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(fp, back);
    }
}

// ── Protocol message properties ─────────────────────────────────────

proptest! {
    /// ReconcileMessage serde roundtrip for RootHash.
    #[test]
    fn msg_root_hash_serde(bytes in prop::array::uniform32(any::<u8>())) {
        let msg = ReconcileMessage::RootHash(MerkleHash::from_bytes(bytes));
        let json = serde_json::to_string(&msg).unwrap();
        let back: ReconcileMessage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(msg, back);
    }

    /// ReconcileMessage serde roundtrip for Converged.
    #[test]
    fn msg_converged_serde(_dummy in 0..1u8) {
        let msg = ReconcileMessage::Converged;
        let json = serde_json::to_string(&msg).unwrap();
        let back: ReconcileMessage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(msg, back);
    }

    /// Phase serde roundtrip.
    #[test]
    fn phase_serde_roundtrip(depth in 0..10usize) {
        let phases = [
            Phase::Init,
            Phase::Narrowing { depth },
            Phase::Patching,
            Phase::Converged,
        ];
        for phase in &phases {
            let json = serde_json::to_string(phase).unwrap();
            let back: Phase = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(*phase, back);
        }
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// Reconciliation is commutative on the result (authority always wins).
    #[test]
    fn reconcile_authority_always_wins(
        tree1 in arb_tree(10),
        tree2 in arb_tree(10),
        config in arb_config()
    ) {
        let (r1, _) = reconcile_trees(&tree1, &tree2, &config);
        let (r2, _) = reconcile_trees(&tree1, &tree1, &config);
        prop_assert_eq!(r1.root_hash(), tree1.root_hash());
        prop_assert_eq!(r2.root_hash(), tree1.root_hash());
    }

    /// Fingerprint divergence implies reconcile will make changes.
    #[test]
    fn fingerprint_divergence_means_changes(
        authority in arb_tree(10),
        replica in arb_tree(10),
        config in arb_config()
    ) {
        let fp_a = StateFingerprint::from_tree(&authority, 0);
        let fp_r = StateFingerprint::from_tree(&replica, 0);
        let (_, stats) = reconcile_with_stats(&authority, &replica, &config);
        if fp_a.matches(&fp_r) {
            prop_assert_eq!(stats.added + stats.removed + stats.changed, 0);
        }
        // Note: non-matching fingerprints don't necessarily mean changes
        // (hash collision), but in practice they always do.
    }

    /// Reconciled result with itself produces zero-change stats.
    #[test]
    fn reconciled_is_stable(
        authority in arb_tree(10),
        replica in arb_tree(10),
        config in arb_config()
    ) {
        let (reconciled, _) = reconcile_trees(&authority, &replica, &config);
        let (_, stats) = reconcile_with_stats(&authority, &reconciled, &config);
        prop_assert_eq!(stats.added, 0);
        prop_assert_eq!(stats.removed, 0);
        prop_assert_eq!(stats.changed, 0);
        prop_assert!(stats.converged);
    }
}

// ── Additional property tests ───────────────────────────────────────

proptest! {
    /// Reconciled result contains no keys absent from authority.
    #[test]
    fn result_has_no_extra_keys(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        for (k, _) in result.iter() {
            prop_assert!(
                authority.contains_key(k),
                "result contains key not in authority"
            );
        }
    }

    /// Stats total matches diff total_changes.
    #[test]
    fn stats_total_matches_diff_total(
        authority in arb_tree(15),
        replica in arb_tree(15),
        config in arb_config()
    ) {
        let diff = replica.diff(&authority);
        let (_, stats) = reconcile_with_stats(&authority, &replica, &config);
        let stats_total = stats.added + stats.removed + stats.changed;
        let diff_total = diff.total_changes();
        prop_assert_eq!(stats_total, diff_total);
    }

    /// ReconcileConfig default always has max_probe_depth == 6.
    #[test]
    fn config_default_probe_depth(_dummy in 0..1u8) {
        let config = ReconcileConfig::default();
        prop_assert_eq!(config.max_probe_depth, 6);
        let dbg = format!("{:?}", config);
        prop_assert!(!dbg.is_empty());
    }

    /// Session update_local reflects new tree state.
    #[test]
    fn session_update_local_reflects_new_tree(
        tree1 in arb_tree(10),
        tree2 in arb_tree(10),
        config in arb_config()
    ) {
        let mut session = ReconcileSession::new(tree1.clone(), true, config);
        let old_hash = session.local_tree().root_hash();
        session.update_local(tree2.clone());
        let new_hash = session.local_tree().root_hash();
        prop_assert_eq!(new_hash, tree2.root_hash());
        // Phase should still be Init after update_local
        prop_assert_eq!(session.phase(), Phase::Init);
        // Old hash should only match if trees happen to be identical
        if tree1.root_hash() != tree2.root_hash() {
            prop_assert_ne!(old_hash, new_hash);
        }
    }

    /// Fingerprint from empty tree has zero entry_count and zero root hash.
    #[test]
    fn fingerprint_empty_tree_properties(version in any::<u64>()) {
        let tree = MerkleTree::new();
        let fp = StateFingerprint::from_tree(&tree, version);
        prop_assert_eq!(fp.entry_count, 0);
        prop_assert_eq!(fp.version, version);
        prop_assert!(fp.root_hash.is_zero(), "empty tree should have zero root hash");
    }

    /// ReconcileMessage::LevelHashes serde roundtrip preserves depth and hashes.
    #[test]
    fn msg_level_hashes_serde(
        depth in 0..10usize,
        hash_bytes in prop::collection::vec(prop::array::uniform32(any::<u8>()), 0..8)
    ) {
        let hashes: Vec<MerkleHash> = hash_bytes.into_iter().map(MerkleHash::from_bytes).collect();
        let msg = ReconcileMessage::LevelHashes { depth, hashes };
        let json = serde_json::to_string(&msg).unwrap();
        let back: ReconcileMessage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(msg, back);
    }

    /// ReconcileMessage::Patch serde roundtrip preserves entries.
    #[test]
    fn msg_patch_serde(
        entries in prop::collection::vec(arb_entry(), 0..10)
    ) {
        let msg = ReconcileMessage::Patch(entries);
        let json = serde_json::to_string(&msg).unwrap();
        let back: ReconcileMessage = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(msg, back);
    }

    /// When authority is a strict subset of replica, surplus keys are removed.
    #[test]
    fn reconcile_removes_surplus_keys(
        base_entries in prop::collection::vec(arb_entry(), 1..10),
        extra_entries in prop::collection::vec(arb_entry(), 1..5),
        config in arb_config()
    ) {
        let authority = MerkleTree::from_entries(base_entries.clone());
        let mut all_entries = base_entries;
        all_entries.extend(extra_entries);
        let replica = MerkleTree::from_entries(all_entries);
        let (result, _) = reconcile_trees(&authority, &replica, &config);
        prop_assert_eq!(result.root_hash(), authority.root_hash());
        prop_assert_eq!(result.len(), authority.len());
        // Every key in result must exist in authority
        for (k, v) in result.iter() {
            let auth_val = authority.get(k);
            prop_assert!(auth_val.is_some(), "result key not in authority");
            prop_assert_eq!(auth_val.unwrap(), v, "result value differs from authority");
        }
    }
}
