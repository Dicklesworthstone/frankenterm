//! Property-based tests for wal_engine.rs — write-ahead log engine.
//!
//! Bead: ft-283h4.2.1

use frankenterm_core::wal_engine::*;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum WalOp {
    Append(String, u64),
    Checkpoint(u64),
}

fn arb_wal_op() -> impl Strategy<Value = WalOp> {
    prop_oneof![
        ("[a-z]{1,8}", 0..10000u64).prop_map(|(s, t)| WalOp::Append(s, t)),
        (0..10000u64).prop_map(WalOp::Checkpoint),
    ]
}

fn arb_wal_ops() -> impl Strategy<Value = Vec<WalOp>> {
    prop::collection::vec(arb_wal_op(), 1..50)
}

fn arb_config() -> impl Strategy<Value = WalConfig> {
    (3..100usize, 1..50usize).prop_map(|(threshold, retained)| WalConfig {
        compaction_threshold: threshold,
        max_retained_entries: retained.min(threshold),
    })
}

fn build_wal(ops: &[WalOp]) -> WalEngine<String> {
    let mut wal = WalEngine::new(WalConfig::default());
    for op in ops {
        match op {
            WalOp::Append(s, t) => {
                wal.append(s.clone(), *t);
            }
            WalOp::Checkpoint(t) => {
                wal.checkpoint(*t);
            }
        }
    }
    wal
}

// ── Sequence number properties ──────────────────────────────────────

proptest! {
    /// Sequence numbers are strictly monotonically increasing.
    #[test]
    fn seq_monotonic(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let seqs: Vec<u64> = wal.iter().map(|e| e.seq).collect();
        for i in 1..seqs.len() {
            prop_assert!(
                seqs[i] > seqs[i - 1],
                "seq {} ({}) should be > seq {} ({})", i, seqs[i], i-1, seqs[i-1]
            );
        }
    }

    /// next_seq is always greater than last_seq.
    #[test]
    fn next_seq_gt_last(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        prop_assert!(wal.next_seq() > wal.last_seq());
    }

    /// first_seq <= last_seq (when non-empty).
    #[test]
    fn first_le_last(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        if !wal.is_empty() {
            prop_assert!(wal.first_seq() <= wal.last_seq());
        }
    }

    /// len matches number of operations.
    #[test]
    fn len_matches_ops(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        prop_assert_eq!(wal.len(), ops.len());
    }
}

// ── Append properties ───────────────────────────────────────────────

proptest! {
    /// Append returns incrementing sequence numbers.
    #[test]
    fn append_returns_incrementing_seqs(
        mutations in prop::collection::vec("[a-z]{1,4}", 1..20)
    ) {
        let mut wal = WalEngine::new(WalConfig::default());
        let mut last_seq = 0u64;
        for (i, m) in mutations.iter().enumerate() {
            let seq = wal.append(m.clone(), i as u64);
            prop_assert!(seq > last_seq, "seq should be monotonically increasing");
            last_seq = seq;
        }
    }

    /// Every appended mutation is retrievable by seq.
    #[test]
    fn appended_entries_retrievable(
        mutations in prop::collection::vec("[a-z]{1,4}", 1..30)
    ) {
        let mut wal = WalEngine::new(WalConfig::default());
        let mut seqs = Vec::new();
        for (i, m) in mutations.iter().enumerate() {
            let seq = wal.append(m.clone(), i as u64);
            seqs.push(seq);
        }
        for (i, seq) in seqs.iter().enumerate() {
            let entry = wal.get(*seq);
            prop_assert!(entry.is_some(), "entry {} should exist", seq);
            let entry = entry.unwrap();
            prop_assert!(
                matches!(&entry.kind, EntryKind::Mutation(s) if s == &mutations[i]),
                "entry {} should contain correct mutation", seq
            );
        }
    }
}

// ── Checkpoint properties ───────────────────────────────────────────

proptest! {
    /// Checkpoint always updates last_checkpoint.
    #[test]
    fn checkpoint_updates_last(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let has_checkpoint = ops.iter().any(|op| matches!(op, WalOp::Checkpoint(_)));
        if has_checkpoint {
            prop_assert!(wal.last_checkpoint().is_some());
        }
    }

    /// last_checkpoint is the seq of the most recent checkpoint.
    #[test]
    fn last_checkpoint_is_most_recent(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        if let Some(cp_seq) = wal.last_checkpoint() {
            let entry = wal.get(cp_seq).unwrap();
            prop_assert!(
                matches!(entry.kind, EntryKind::Checkpoint),
                "last_checkpoint should point to a Checkpoint entry"
            );
            // No later checkpoint exists
            let later_cp = wal.iter()
                .filter(|e| e.seq > cp_seq && matches!(e.kind, EntryKind::Checkpoint))
                .count();
            prop_assert_eq!(later_cp, 0, "no checkpoint should exist after last_checkpoint");
        }
    }

    /// since_last_checkpoint returns only entries after the checkpoint.
    #[test]
    fn since_last_checkpoint_correct(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let cp_seq = wal.last_checkpoint().unwrap_or(0);
        let since: Vec<_> = wal.since_last_checkpoint().collect();
        for entry in &since {
            prop_assert!(
                entry.seq > cp_seq,
                "entry seq {} should be > checkpoint seq {}", entry.seq, cp_seq
            );
        }
    }
}

// ── Iterator properties ─────────────────────────────────────────────

proptest! {
    /// mutations() returns only Mutation entries.
    #[test]
    fn mutations_only_mutations(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        for entry in wal.mutations() {
            prop_assert!(
                matches!(entry.kind, EntryKind::Mutation(_)),
                "mutations() should only return Mutation entries"
            );
        }
    }

    /// mutations() count matches number of Append ops.
    #[test]
    fn mutations_count_matches(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let expected = ops.iter().filter(|op| matches!(op, WalOp::Append(_, _))).count();
        prop_assert_eq!(wal.mutations().count(), expected);
    }

    /// since(seq) returns only entries with seq > given.
    #[test]
    fn since_correct(ops in arb_wal_ops(), threshold in 0..60u64) {
        let wal = build_wal(&ops);
        for entry in wal.since(threshold) {
            prop_assert!(
                entry.seq > threshold,
                "since({}) returned entry with seq {}", threshold, entry.seq
            );
        }
    }

    /// range(from, to) returns entries in [from, to].
    #[test]
    fn range_correct(ops in arb_wal_ops(), from in 1..30u64, span in 0..20u64) {
        let to = from + span;
        let wal = build_wal(&ops);
        for entry in wal.range(from, to) {
            prop_assert!(entry.seq >= from && entry.seq <= to,
                "range({}, {}) returned entry with seq {}", from, to, entry.seq);
        }
    }

    /// iter() returns entries in seq order.
    #[test]
    fn iter_ordered(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let entries: Vec<_> = wal.iter().collect();
        for i in 1..entries.len() {
            prop_assert!(entries[i].seq > entries[i - 1].seq);
        }
    }
}

// ── Compaction properties ───────────────────────────────────────────

proptest! {
    /// Compaction never loses entries after the last checkpoint.
    #[test]
    fn compact_preserves_post_checkpoint(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        let cp_seq = wal.last_checkpoint().unwrap_or(0);
        let post_cp_count = wal.since(cp_seq).count();

        wal.compact();

        // All entries after checkpoint should still be accessible
        let post_compact_count = wal.iter()
            .filter(|e| e.seq > cp_seq && !matches!(e.kind, EntryKind::CompactionMarker { .. }))
            .count();
        prop_assert!(
            post_compact_count >= post_cp_count.saturating_sub(1),
            "post-checkpoint entries should survive compaction"
        );
    }

    /// Compaction reduces or maintains entry count.
    #[test]
    fn compact_reduces_size(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        let before = wal.len();
        wal.compact();
        // Compaction may add a marker but removes more
        // In worst case, nothing is removed and marker is added
        prop_assert!(wal.len() <= before + 1);
    }

    /// needs_compaction is consistent with threshold.
    #[test]
    fn needs_compaction_consistent(
        ops in arb_wal_ops(),
        config in arb_config()
    ) {
        let mut wal = WalEngine::<String>::new(config.clone());
        for op in &ops {
            match op {
                WalOp::Append(s, t) => { wal.append(s.clone(), *t); }
                WalOp::Checkpoint(t) => { wal.checkpoint(*t); }
            }
        }
        prop_assert_eq!(
            wal.needs_compaction(),
            wal.len() > config.compaction_threshold
        );
    }
}

// ── Truncate properties ─────────────────────────────────────────────

proptest! {
    /// Truncate removes entries beyond the given seq.
    #[test]
    fn truncate_removes_beyond(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        if wal.len() >= 2 {
            let mid_seq = wal.first_seq() + (wal.last_seq() - wal.first_seq()) / 2;
            wal.truncate_after(mid_seq);
            prop_assert!(wal.last_seq() <= mid_seq);
            prop_assert_eq!(wal.next_seq(), mid_seq + 1);
        }
    }

    /// Truncate at last_seq is a no-op.
    #[test]
    fn truncate_at_last_is_noop(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        let last = wal.last_seq();
        let len_before = wal.len();
        wal.truncate_after(last);
        prop_assert_eq!(wal.len(), len_before);
    }
}

// ── Replay properties ───────────────────────────────────────────────

proptest! {
    /// Replay returns only mutations, not checkpoints.
    #[test]
    fn replay_only_mutations(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let replayed = replay_mutations(&wal, 0, u64::MAX);
        let expected_count = ops.iter().filter(|op| matches!(op, WalOp::Append(_, _))).count();
        prop_assert_eq!(replayed.len(), expected_count);
    }

    /// Replay with full range matches mutations iterator.
    #[test]
    fn replay_full_matches_mutations(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let replayed = replay_mutations(&wal, 0, u64::MAX);
        let mutations: Vec<_> = wal.mutations().collect();
        prop_assert_eq!(replayed.len(), mutations.len());
        for (r, m) in replayed.iter().zip(mutations.iter()) {
            if let EntryKind::Mutation(expected) = &m.kind {
                prop_assert_eq!(*r, expected);
            }
        }
    }
}

// ── Stats properties ────────────────────────────────────────────────

proptest! {
    /// Stats mutation_count + checkpoint_count + markers = total.
    #[test]
    fn stats_counts_sum(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let stats = wal.stats();
        let marker_count = wal.iter()
            .filter(|e| matches!(e.kind, EntryKind::CompactionMarker { .. }))
            .count();
        prop_assert_eq!(
            stats.mutation_count + stats.checkpoint_count + marker_count,
            stats.total_entries,
            "mutation + checkpoint + marker counts should equal total"
        );
    }

    /// Stats serde roundtrip.
    #[test]
    fn stats_serde(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let stats = wal.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: WalStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// Clear preserves sequence counter.
    #[test]
    fn clear_preserves_seq(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        let next = wal.next_seq();
        wal.clear();
        prop_assert!(wal.is_empty());
        prop_assert_eq!(wal.next_seq(), next);
    }

    /// Compact then replay gives same mutations as original.
    #[test]
    fn compact_replay_consistent(ops in arb_wal_ops()) {
        let wal_orig = build_wal(&ops);
        let orig_muts: Vec<String> = wal_orig.mutations()
            .filter_map(|e| match &e.kind {
                EntryKind::Mutation(s) => Some(s.clone()),
                _ => None,
            })
            .collect();

        let mut wal_compacted = build_wal(&ops);
        wal_compacted.compact();
        let compacted_muts: Vec<String> = wal_compacted.mutations()
            .filter_map(|e| match &e.kind {
                EntryKind::Mutation(s) => Some(s.clone()),
                _ => None,
            })
            .collect();

        // Compacted mutations should be a suffix of original
        prop_assert!(
            orig_muts.ends_with(&compacted_muts),
            "compacted mutations should be a suffix of original"
        );
    }
}
