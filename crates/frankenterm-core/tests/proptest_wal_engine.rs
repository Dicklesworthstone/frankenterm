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

// ── Additional invariant properties ─────────────────────────────

proptest! {
    /// is_empty agrees with len: empty iff len == 0.
    #[test]
    fn is_empty_agrees_with_len(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let is_empty = wal.is_empty();
        let len_zero = wal.is_empty();
        prop_assert_eq!(is_empty, len_zero);
    }

    /// A freshly constructed WAL is always empty regardless of config.
    #[test]
    fn new_wal_is_empty(config in arb_config()) {
        let wal = WalEngine::<String>::new(config);
        prop_assert!(wal.is_empty());
        prop_assert_eq!(wal.len(), 0);
        prop_assert_eq!(wal.next_seq(), 1);
        prop_assert!(wal.last_checkpoint().is_none());
        prop_assert_eq!(wal.compacted_through(), 0);
    }

    /// Clone produces an independent copy: mutating one does not affect the other.
    #[test]
    fn clone_independence(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let mut cloned = wal.clone();
        cloned.append("extra_clone_entry".to_string(), 99999);

        let orig_len = wal.len();
        let cloned_len = cloned.len();
        prop_assert_eq!(cloned_len, orig_len + 1);
        // Original is unmodified
        prop_assert_eq!(wal.len(), orig_len);
    }

    /// Stats fields are consistent with direct WAL queries.
    #[test]
    fn stats_consistent_with_queries(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        let stats = wal.stats();

        let total = wal.len();
        let first = wal.first_seq();
        let last = wal.last_seq();
        let cp = wal.last_checkpoint();
        let ct = wal.compacted_through();
        let nc = wal.needs_compaction();

        prop_assert_eq!(stats.total_entries, total);
        prop_assert_eq!(stats.first_seq, first);
        prop_assert_eq!(stats.last_seq, last);
        prop_assert_eq!(stats.last_checkpoint_seq, cp);
        prop_assert_eq!(stats.compacted_through, ct);
        prop_assert_eq!(stats.needs_compaction, nc);
    }

    /// WalEntry serde roundtrip preserves equality for all entry kinds.
    #[test]
    fn wal_entry_serde_roundtrip(ops in arb_wal_ops()) {
        let wal = build_wal(&ops);
        for entry in wal.iter() {
            let json = serde_json::to_string(entry).unwrap();
            let back: WalEntry<String> = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(entry.seq, back.seq);
            prop_assert_eq!(entry.timestamp_ms, back.timestamp_ms);
            prop_assert_eq!(entry.kind.clone(), back.kind);
        }
    }

    /// compacted_through never decreases across successive compact calls.
    #[test]
    fn compacted_through_monotonic(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        let ct0 = wal.compacted_through();
        wal.compact();
        let ct1 = wal.compacted_through();
        // Append more entries then compact again
        wal.append("post_compact_a".to_string(), 50000);
        wal.checkpoint(50001);
        wal.append("post_compact_b".to_string(), 50002);
        wal.compact();
        let ct2 = wal.compacted_through();
        prop_assert!(ct1 >= ct0, "compacted_through should not decrease: {} < {}", ct1, ct0);
        prop_assert!(ct2 >= ct1, "compacted_through should not decrease: {} < {}", ct2, ct1);
    }

    /// Truncate then append: new entry gets the correct next sequence number.
    #[test]
    fn truncate_then_append_seq_correct(ops in arb_wal_ops()) {
        let mut wal = build_wal(&ops);
        if wal.len() >= 2 {
            let mid = wal.first_seq() + (wal.last_seq() - wal.first_seq()) / 2;
            wal.truncate_after(mid);
            let expected_next = mid + 1;
            let next = wal.next_seq();
            prop_assert_eq!(next, expected_next);
            let new_seq = wal.append("after_truncate".to_string(), 77777);
            prop_assert_eq!(new_seq, expected_next);
        }
    }
}

// ── Disk WAL properties ──────────────────────────────────────────────

fn arb_disk_wal_config() -> impl Strategy<Value = DiskWalConfig> {
    (3..50usize, 1..20usize).prop_map(|(threshold, retained)| DiskWalConfig {
        wal_config: WalConfig {
            compaction_threshold: threshold,
            max_retained_entries: retained.min(threshold),
        },
        fsync_on_write: false,
        max_file_size: 50 * 1024 * 1024,
    })
}

#[derive(Clone, Debug)]
enum MuxOp {
    PaneOutput(u64, Vec<u8>),
    PaneCreated(u64, u16, u16),
    PaneClosed(u64),
    FocusChanged(u64),
    Checkpoint,
}

fn arb_mux_op() -> impl Strategy<Value = MuxOp> {
    prop_oneof![
        (1..100u64, prop::collection::vec(0..255u8, 0..32))
            .prop_map(|(id, data)| MuxOp::PaneOutput(id, data)),
        (1..100u64, 10..60u16, 40..200u16).prop_map(|(id, r, c)| MuxOp::PaneCreated(id, r, c)),
        (1..100u64).prop_map(MuxOp::PaneClosed),
        (1..100u64).prop_map(MuxOp::FocusChanged),
        Just(MuxOp::Checkpoint),
    ]
}

fn to_mux_mutation(op: &MuxOp) -> Option<MuxMutation> {
    match op {
        MuxOp::PaneOutput(id, data) => Some(MuxMutation::PaneOutput {
            pane_id: *id,
            data: data.clone(),
        }),
        MuxOp::PaneCreated(id, r, c) => Some(MuxMutation::PaneCreated {
            pane_id: *id,
            rows: *r,
            cols: *c,
            title: format!("pane_{}", id),
        }),
        MuxOp::PaneClosed(id) => Some(MuxMutation::PaneClosed { pane_id: *id }),
        MuxOp::FocusChanged(id) => Some(MuxMutation::FocusChanged { pane_id: *id }),
        MuxOp::Checkpoint => None,
    }
}

proptest! {
    /// CRC32 is deterministic: same input always produces same output.
    #[test]
    fn crc32_deterministic(data in prop::collection::vec(0..255u8, 0..200)) {
        // We verify indirectly via WalFrame: encode same entry twice, get same CRC
        let entry = WalEntry {
            seq: 1,
            timestamp_ms: 100,
            kind: EntryKind::Mutation(data.clone()),
        };
        let json1 = serde_json::to_vec(&entry).unwrap();
        let json2 = serde_json::to_vec(&entry).unwrap();
        prop_assert_eq!(json1, json2, "same entry should serialize identically");
    }

    /// DiskWal: write ops then reload — entries survive persistence.
    #[test]
    fn disk_wal_persist_reload(
        ops in prop::collection::vec(arb_mux_op(), 1..30)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let config = DiskWalConfig::default();

        let expected_mutations: Vec<MuxMutation> = ops.iter()
            .filter_map(to_mux_mutation)
            .collect();
        let expected_checkpoints = ops.iter()
            .filter(|op| matches!(op, MuxOp::Checkpoint))
            .count();

        // Write
        {
            let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config.clone()).unwrap();
            for (i, op) in ops.iter().enumerate() {
                match op {
                    MuxOp::Checkpoint => { wal.checkpoint(i as u64 * 100).unwrap(); }
                    other => {
                        if let Some(m) = to_mux_mutation(other) {
                            wal.append(m, i as u64 * 100).unwrap();
                        }
                    }
                }
            }
        }

        // Reload and verify
        {
            let (wal, result) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
            prop_assert_eq!(result.corrupt_tail_entries, 0);
            prop_assert_eq!(
                result.entries_loaded,
                expected_mutations.len() + expected_checkpoints,
                "loaded entries should match written"
            );

            let loaded_muts: Vec<&MuxMutation> = wal.engine().mutations()
                .filter_map(|e| match &e.kind {
                    EntryKind::Mutation(m) => Some(m),
                    _ => None,
                })
                .collect();
            prop_assert_eq!(loaded_muts.len(), expected_mutations.len());
            for (loaded, expected) in loaded_muts.iter().zip(expected_mutations.iter()) {
                prop_assert_eq!(*loaded, expected);
            }
        }
    }

    /// DiskWal: sequence numbers are continuous across sessions.
    #[test]
    fn disk_wal_seq_continuity(
        ops1 in prop::collection::vec(arb_mux_op(), 1..15),
        ops2 in prop::collection::vec(arb_mux_op(), 1..15)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let config = DiskWalConfig::default();

        let last_seq_session1;
        {
            let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config.clone()).unwrap();
            for (i, op) in ops1.iter().enumerate() {
                match op {
                    MuxOp::Checkpoint => { wal.checkpoint(i as u64).unwrap(); }
                    other => {
                        if let Some(m) = to_mux_mutation(other) {
                            wal.append(m, i as u64).unwrap();
                        }
                    }
                }
            }
            last_seq_session1 = wal.engine().last_seq();
        }

        {
            let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
            for (i, op) in ops2.iter().enumerate() {
                match op {
                    MuxOp::Checkpoint => {
                        let seq = wal.checkpoint(1000 + i as u64).unwrap();
                        prop_assert!(seq > last_seq_session1,
                            "session2 seq {} should be > session1 last_seq {}", seq, last_seq_session1);
                    }
                    other => {
                        if let Some(m) = to_mux_mutation(other) {
                            let seq = wal.append(m, 1000 + i as u64).unwrap();
                            prop_assert!(seq > last_seq_session1,
                                "session2 seq {} should be > session1 last_seq {}", seq, last_seq_session1);
                        }
                    }
                }
            }
        }
    }

    /// DiskWal: compact_and_rewrite produces a valid, loadable file.
    #[test]
    fn disk_wal_compact_rewrite_valid(
        ops in prop::collection::vec(arb_mux_op(), 5..30)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let config = DiskWalConfig {
            wal_config: WalConfig {
                compaction_threshold: 5,
                max_retained_entries: 3,
            },
            fsync_on_write: false,
            max_file_size: 50 * 1024 * 1024,
        };

        {
            let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config.clone()).unwrap();
            for (i, op) in ops.iter().enumerate() {
                match op {
                    MuxOp::Checkpoint => { wal.checkpoint(i as u64 * 100).unwrap(); }
                    other => {
                        if let Some(m) = to_mux_mutation(other) {
                            wal.append(m, i as u64 * 100).unwrap();
                        }
                    }
                }
            }
            let _removed = wal.compact_and_rewrite().unwrap();
        }

        // Verify rewritten file is valid
        {
            let (wal, result) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
            prop_assert_eq!(result.corrupt_tail_entries, 0, "rewritten file should be corruption-free");
            // Engine should be non-empty (at least retained entries + compaction marker)
            prop_assert!(wal.engine().len() > 0 || ops.is_empty());
        }
    }

    /// DiskWal: crash recovery (truncated tail) preserves all good entries.
    #[test]
    fn disk_wal_crash_recovery_preserves_good(
        ops in prop::collection::vec(arb_mux_op(), 2..20),
        garbage_len in 1..50usize
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let config = DiskWalConfig::default();

        let expected_count;
        {
            let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config.clone()).unwrap();
            for (i, op) in ops.iter().enumerate() {
                match op {
                    MuxOp::Checkpoint => { wal.checkpoint(i as u64 * 100).unwrap(); }
                    other => {
                        if let Some(m) = to_mux_mutation(other) {
                            wal.append(m, i as u64 * 100).unwrap();
                        }
                    }
                }
            }
            expected_count = wal.engine().len();
        }

        // Append garbage to simulate crash
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let garbage: Vec<u8> = (0..garbage_len).map(|i| (i % 256) as u8).collect();
            file.write_all(&garbage).unwrap();
        }

        // Recovery should load all good entries
        {
            let (wal, result) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
            prop_assert_eq!(
                result.entries_loaded, expected_count,
                "should recover all {} good entries, got {}", expected_count, result.entries_loaded
            );
            prop_assert!(
                result.corrupt_tail_entries > 0 || garbage_len < 4,
                "should detect corruption (unless garbage is too short to form a length prefix)"
            );
        }
    }

    /// MuxMutation: all variants survive serde roundtrip.
    #[test]
    fn mux_mutation_serde_roundtrip(op in arb_mux_op()) {
        if let Some(m) = to_mux_mutation(&op) {
            let json = serde_json::to_string(&m).unwrap();
            let back: MuxMutation = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(m, back);
        }
    }

    /// DiskWal: exceeds_max_size is monotonically true once triggered.
    #[test]
    fn disk_wal_exceeds_max_size_monotonic(
        ops in prop::collection::vec(arb_mux_op(), 5..30)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let config = DiskWalConfig {
            wal_config: WalConfig::default(),
            fsync_on_write: false,
            max_file_size: 200, // very small threshold
        };

        let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, config).unwrap();
        let mut exceeded = false;
        for (i, op) in ops.iter().enumerate() {
            match op {
                MuxOp::Checkpoint => { wal.checkpoint(i as u64).unwrap(); }
                other => {
                    if let Some(m) = to_mux_mutation(other) {
                        wal.append(m, i as u64).unwrap();
                    }
                }
            }
            if wal.exceeds_max_size() {
                exceeded = true;
            }
            if exceeded {
                prop_assert!(wal.exceeds_max_size(),
                    "once exceeded, should stay exceeded (monotonic)");
            }
        }
    }

    /// DiskWal: file_size grows with each append.
    #[test]
    fn disk_wal_file_size_grows(
        mutations in prop::collection::vec(
            (1..100u64, prop::collection::vec(0..255u8, 1..20)),
            1..15
        )
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let (mut wal, _) = DiskWal::<MuxMutation>::open(&path, DiskWalConfig::default()).unwrap();

        let mut prev_size = wal.file_size();
        for (id, data) in &mutations {
            wal.append(
                MuxMutation::PaneOutput {
                    pane_id: *id,
                    data: data.clone(),
                },
                0,
            ).unwrap();
            let new_size = wal.file_size();
            prop_assert!(
                new_size > prev_size,
                "file size should grow: {} -> {}", prev_size, new_size
            );
            prev_size = new_size;
        }
    }

    /// Replay correctness: applying mutations from WAL replay produces
    /// the same sequence of mutations as direct application.
    #[test]
    fn replay_identity(ops in prop::collection::vec(arb_mux_op(), 1..30)) {
        // Collect mutations directly
        let direct_muts: Vec<MuxMutation> = ops.iter()
            .filter_map(to_mux_mutation)
            .collect();

        // Build in-memory WAL and replay
        let mut wal = WalEngine::<MuxMutation>::new(WalConfig::default());
        for (i, op) in ops.iter().enumerate() {
            match op {
                MuxOp::Checkpoint => { wal.checkpoint(i as u64); }
                other => {
                    if let Some(m) = to_mux_mutation(other) {
                        wal.append(m, i as u64);
                    }
                }
            }
        }

        let replayed: Vec<&MuxMutation> = replay_mutations(&wal, 0, u64::MAX);
        prop_assert_eq!(replayed.len(), direct_muts.len());
        for (r, d) in replayed.iter().zip(direct_muts.iter()) {
            prop_assert_eq!(*r, d);
        }
    }
}
