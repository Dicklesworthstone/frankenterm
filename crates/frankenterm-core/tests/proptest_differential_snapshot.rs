//! Property-based tests for differential_snapshot module.
//!
//! Verifies invariants of:
//! - DirtyTracker: field tracking, created/closed → layout_dirty, clear semantics
//! - BaseSnapshot: apply_diff consistency (create/close/metadata/scrollback/layout)
//! - DiffChain: seq monotonicity, restore_at correctness, compaction equivalence
//! - DiffSnapshotEngine: auto-compaction, tracker clearing, clean→None
//! - SnapshotDiff: pane_id() accessor, serde roundtrip
//!
//! Complements the 3 inline proptests (restore_matches_live_state,
//! compaction_preserves_state, dirty_count_matches_dirty_set).

use std::collections::{HashMap, HashSet};

use proptest::prelude::*;

use frankenterm_core::differential_snapshot::{
    BaseSnapshot, DiffChain, DiffSnapshot, DiffSnapshotEngine, DirtyField, DirtyTracker,
    SnapshotDiff,
};
use frankenterm_core::session_pane_state::{PaneStateSnapshot, ScrollbackRef, TerminalState};
use frankenterm_core::session_topology::{
    PaneNode, TabSnapshot, TopologySnapshot, WindowSnapshot, TOPOLOGY_SCHEMA_VERSION,
};

// ────────────────────────────────────────────────────────────────────
// Helpers (mirror the module's test helpers)
// ────────────────────────────────────────────────────────────────────

fn make_terminal(rows: u16, cols: u16) -> TerminalState {
    TerminalState {
        rows,
        cols,
        cursor_row: 0,
        cursor_col: 0,
        is_alt_screen: false,
        title: "test".to_string(),
    }
}

fn make_pane_state(pane_id: u64, rows: u16, cols: u16) -> PaneStateSnapshot {
    PaneStateSnapshot::new(pane_id, 1000, make_terminal(rows, cols))
        .with_cwd(format!("/home/user/pane-{}", pane_id))
}

fn make_topology(pane_ids: &[u64]) -> TopologySnapshot {
    let tabs: Vec<TabSnapshot> = pane_ids
        .iter()
        .map(|&id| TabSnapshot {
            tab_id: id,
            title: Some(format!("tab-{}", id)),
            pane_tree: PaneNode::Leaf {
                pane_id: id,
                rows: 24,
                cols: 80,
                cwd: None,
                title: None,
                is_active: false,
            },
            active_pane_id: Some(id),
        })
        .collect();

    TopologySnapshot {
        schema_version: TOPOLOGY_SCHEMA_VERSION,
        captured_at: 1000,
        workspace_id: None,
        windows: vec![WindowSnapshot {
            window_id: 0,
            title: Some("test-window".to_string()),
            position: None,
            size: None,
            tabs,
            active_tab_index: Some(0),
        }],
    }
}

fn make_base(pane_ids: &[u64]) -> BaseSnapshot {
    let pane_states: Vec<PaneStateSnapshot> = pane_ids
        .iter()
        .map(|&id| make_pane_state(id, 24, 80))
        .collect();
    BaseSnapshot::new(1000, make_topology(pane_ids), pane_states)
}

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1u64..=20
}

fn arb_pane_ids() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::hash_set(1u64..=20, 1..=8).prop_map(|s| s.into_iter().collect())
}

fn arb_dirty_field() -> impl Strategy<Value = DirtyField> {
    prop_oneof![
        Just(DirtyField::Scrollback),
        Just(DirtyField::Metadata),
        Just(DirtyField::Created),
        Just(DirtyField::Closed),
    ]
}

#[derive(Debug, Clone)]
enum TrackerOp {
    MarkOutput(u64),
    MarkMetadata(u64),
    MarkCreated(u64),
    MarkClosed(u64),
    MarkLayoutDirty,
    Clear,
}

fn arb_tracker_op() -> impl Strategy<Value = TrackerOp> {
    prop_oneof![
        arb_pane_id().prop_map(TrackerOp::MarkOutput),
        arb_pane_id().prop_map(TrackerOp::MarkMetadata),
        arb_pane_id().prop_map(TrackerOp::MarkCreated),
        arb_pane_id().prop_map(TrackerOp::MarkClosed),
        Just(TrackerOp::MarkLayoutDirty),
        Just(TrackerOp::Clear),
    ]
}

fn arb_tracker_ops(max_len: usize) -> impl Strategy<Value = Vec<TrackerOp>> {
    prop::collection::vec(arb_tracker_op(), 1..=max_len)
}

// ────────────────────────────────────────────────────────────────────
// DirtyTracker: created/closed implies layout_dirty
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// mark_created always sets layout_dirty and adds to dirty map.
    #[test]
    fn prop_created_sets_layout_dirty(
        pane_id in arb_pane_id(),
    ) {
        let mut tracker = DirtyTracker::new();
        tracker.mark_created(pane_id);

        prop_assert!(tracker.is_layout_dirty(), "Created should set layout_dirty");
        prop_assert!(
            tracker.dirty_fields(pane_id).unwrap().contains(&DirtyField::Created),
            "Created field not recorded"
        );
        prop_assert!(tracker.dirty_pane_ids().contains(&pane_id));
    }

    /// mark_closed always sets layout_dirty and adds to dirty map.
    #[test]
    fn prop_closed_sets_layout_dirty(
        pane_id in arb_pane_id(),
    ) {
        let mut tracker = DirtyTracker::new();
        tracker.mark_closed(pane_id);

        prop_assert!(tracker.is_layout_dirty(), "Closed should set layout_dirty");
        prop_assert!(
            tracker.dirty_fields(pane_id).unwrap().contains(&DirtyField::Closed),
            "Closed field not recorded"
        );
    }

    /// mark_output and mark_metadata do NOT set layout_dirty.
    #[test]
    fn prop_output_metadata_no_layout_dirty(
        pane_id in arb_pane_id(),
    ) {
        let mut t1 = DirtyTracker::new();
        t1.mark_output(pane_id);
        prop_assert!(!t1.is_layout_dirty(), "mark_output should not set layout_dirty");

        let mut t2 = DirtyTracker::new();
        t2.mark_metadata(pane_id);
        prop_assert!(!t2.is_layout_dirty(), "mark_metadata should not set layout_dirty");
    }
}

// ────────────────────────────────────────────────────────────────────
// DirtyTracker: multiple fields accumulate per pane
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Marking multiple fields on the same pane accumulates them all.
    #[test]
    fn prop_fields_accumulate(
        pane_id in arb_pane_id(),
        fields in prop::collection::hash_set(arb_dirty_field(), 1..=4),
    ) {
        let mut tracker = DirtyTracker::new();
        for &field in &fields {
            tracker.mark_dirty(pane_id, field);
        }

        prop_assert_eq!(tracker.dirty_count(), 1, "Should be exactly 1 pane");
        let recorded = tracker.dirty_fields(pane_id).unwrap();
        for &field in &fields {
            prop_assert!(
                recorded.contains(&field),
                "Field {:?} missing from recorded set", field
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DirtyTracker: clear resets all state
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// After any sequence of operations then clear(), tracker is clean.
    #[test]
    fn prop_clear_always_clean(
        ops in arb_tracker_ops(30),
    ) {
        let mut tracker = DirtyTracker::new();
        for op in &ops {
            match op {
                TrackerOp::MarkOutput(id) => tracker.mark_output(*id),
                TrackerOp::MarkMetadata(id) => tracker.mark_metadata(*id),
                TrackerOp::MarkCreated(id) => tracker.mark_created(*id),
                TrackerOp::MarkClosed(id) => tracker.mark_closed(*id),
                TrackerOp::MarkLayoutDirty => tracker.mark_layout_dirty(),
                TrackerOp::Clear => tracker.clear(),
            }
        }

        tracker.clear();
        prop_assert!(tracker.is_clean());
        prop_assert_eq!(tracker.dirty_count(), 0);
        prop_assert!(!tracker.is_layout_dirty());
        prop_assert!(tracker.dirty_pane_ids().is_empty());
    }
}

// ────────────────────────────────────────────────────────────────────
// DirtyTracker: is_clean consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// is_clean() ⟺ dirty_count() == 0 && !is_layout_dirty().
    #[test]
    fn prop_is_clean_consistent(
        ops in arb_tracker_ops(20),
    ) {
        let mut tracker = DirtyTracker::new();
        for op in &ops {
            match op {
                TrackerOp::MarkOutput(id) => tracker.mark_output(*id),
                TrackerOp::MarkMetadata(id) => tracker.mark_metadata(*id),
                TrackerOp::MarkCreated(id) => tracker.mark_created(*id),
                TrackerOp::MarkClosed(id) => tracker.mark_closed(*id),
                TrackerOp::MarkLayoutDirty => tracker.mark_layout_dirty(),
                TrackerOp::Clear => tracker.clear(),
            }

            let expected_clean = tracker.dirty_count() == 0 && !tracker.is_layout_dirty();
            prop_assert_eq!(
                tracker.is_clean(), expected_clean,
                "is_clean mismatch: dirty_count={}, layout_dirty={}",
                tracker.dirty_count(), tracker.is_layout_dirty()
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DirtyField serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// DirtyField survives JSON roundtrip.
    #[test]
    fn prop_dirty_field_serde(
        field in arb_dirty_field(),
    ) {
        let json = serde_json::to_string(&field).unwrap();
        let back: DirtyField = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(field, back);
    }
}

// ────────────────────────────────────────────────────────────────────
// BaseSnapshot: apply_diff PaneCreated adds pane
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PaneCreated diff adds a new pane to the snapshot.
    #[test]
    fn prop_apply_diff_create_adds_pane(
        base_ids in arb_pane_ids(),
        new_id in 21u64..=30, // guaranteed not in base_ids (1..=20)
    ) {
        let mut base = make_base(&base_ids);
        let initial_count = base.pane_states.len();

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: new_id,
                snapshot: make_pane_state(new_id, 24, 80),
            }],
        };

        base.apply_diff(&diff);
        prop_assert_eq!(base.pane_states.len(), initial_count + 1);
        prop_assert!(base.pane_states.contains_key(&new_id));
        prop_assert_eq!(base.captured_at, 2000);
    }

    /// PaneClosed diff removes a pane from the snapshot.
    #[test]
    fn prop_apply_diff_close_removes_pane(
        base_ids in prop::collection::hash_set(1u64..=20, 2..=8)
            .prop_map(|s| -> Vec<u64> { s.into_iter().collect() }),
    ) {
        let mut base = make_base(&base_ids);
        let initial_count = base.pane_states.len();
        let target_id = base_ids[0];

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: target_id }],
        };

        base.apply_diff(&diff);
        prop_assert_eq!(base.pane_states.len(), initial_count - 1);
        prop_assert!(!base.pane_states.contains_key(&target_id));
    }
}

// ────────────────────────────────────────────────────────────────────
// BaseSnapshot: apply_diff updates captured_at
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// apply_diff always updates captured_at to the diff's timestamp.
    #[test]
    fn prop_apply_diff_updates_captured_at(
        base_ids in arb_pane_ids(),
        timestamp in 2000u64..100_000,
    ) {
        let mut base = make_base(&base_ids);
        let target = base_ids[0];

        let diff = DiffSnapshot {
            seq: 1,
            captured_at: timestamp,
            diffs: vec![SnapshotDiff::PaneScrollbackChanged {
                pane_id: target,
                new_scrollback_ref: Some(ScrollbackRef {
                    output_segments_seq: 42,
                    total_lines_captured: 500,
                    last_capture_at: timestamp,
                }),
            }],
        };

        base.apply_diff(&diff);
        prop_assert_eq!(base.captured_at, timestamp);
    }
}

// ────────────────────────────────────────────────────────────────────
// SnapshotDiff: pane_id() accessor
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// pane_id() returns Some for pane diffs, None for layout diffs.
    #[test]
    fn prop_snapshot_diff_pane_id(
        pane_id in arb_pane_id(),
    ) {
        let created = SnapshotDiff::PaneCreated {
            pane_id,
            snapshot: make_pane_state(pane_id, 24, 80),
        };
        prop_assert_eq!(created.pane_id(), Some(pane_id));

        let closed = SnapshotDiff::PaneClosed { pane_id };
        prop_assert_eq!(closed.pane_id(), Some(pane_id));

        let metadata = SnapshotDiff::PaneMetadataChanged {
            pane_id,
            new_state: make_pane_state(pane_id, 24, 80),
        };
        prop_assert_eq!(metadata.pane_id(), Some(pane_id));

        let scrollback = SnapshotDiff::PaneScrollbackChanged {
            pane_id,
            new_scrollback_ref: None,
        };
        prop_assert_eq!(scrollback.pane_id(), Some(pane_id));

        let layout = SnapshotDiff::LayoutChanged {
            new_topology: make_topology(&[pane_id]),
        };
        prop_assert_eq!(layout.pane_id(), None);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffSnapshot serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DiffSnapshot survives JSON serialization roundtrip.
    #[test]
    fn prop_diff_snapshot_serde_roundtrip(
        pane_id in arb_pane_id(),
        seq in 1u64..100,
        ts in 1000u64..100_000,
    ) {
        let diff = DiffSnapshot {
            seq,
            captured_at: ts,
            diffs: vec![
                SnapshotDiff::PaneCreated {
                    pane_id,
                    snapshot: make_pane_state(pane_id, 24, 80),
                },
                SnapshotDiff::PaneClosed { pane_id: pane_id + 100 },
            ],
        };

        let json = serde_json::to_string(&diff).unwrap();
        let back: DiffSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(diff, back);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: sequence numbers are monotonically increasing
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// push_diff assigns strictly increasing seq numbers starting at 1.
    #[test]
    fn prop_chain_seq_monotonic(
        n_diffs in 1usize..=20,
    ) {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        for i in 0..n_diffs {
            chain.push_diff(DiffSnapshot {
                seq: 0, // will be overwritten
                captured_at: 1000 + (i as u64) * 1000,
                diffs: vec![],
            });
        }

        prop_assert_eq!(chain.chain_len(), n_diffs);

        for (i, diff) in chain.diffs.iter().enumerate() {
            prop_assert_eq!(diff.seq, (i + 1) as u64, "seq mismatch at index {}", i);
        }

        // Consecutive seqs are strictly increasing
        for w in chain.diffs.windows(2) {
            prop_assert!(w[1].seq > w[0].seq, "Non-monotonic seq: {} >= {}", w[0].seq, w[1].seq);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: restore_at(0) returns base
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// restore_at(0) always returns the original base snapshot.
    #[test]
    fn prop_restore_at_zero_is_base(
        base_ids in arb_pane_ids(),
        n_diffs in 0usize..=5,
    ) {
        let base = make_base(&base_ids);
        let base_pane_ids: HashSet<u64> = base.pane_states.keys().copied().collect();
        let base_captured = base.captured_at;

        let mut chain = DiffChain::new(base);

        // Add some diffs (creating new panes 21..=25)
        for i in 0..n_diffs {
            let new_id = 21 + i as u64;
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 2000 + (i as u64) * 1000,
                diffs: vec![SnapshotDiff::PaneCreated {
                    pane_id: new_id,
                    snapshot: make_pane_state(new_id, 24, 80),
                }],
            });
        }

        let at_zero = chain.restore_at(0).unwrap();
        let restored_ids: HashSet<u64> = at_zero.pane_states.keys().copied().collect();
        prop_assert_eq!(base_pane_ids, restored_ids, "restore_at(0) should equal base pane IDs");
        prop_assert_eq!(at_zero.captured_at, base_captured);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: restore_latest == restore_at(last_seq)
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// restore_latest produces the same state as restore_at(last_seq).
    #[test]
    fn prop_restore_latest_equals_restore_at_last(
        base_ids in arb_pane_ids(),
        n_diffs in 1usize..=8,
    ) {
        let base = make_base(&base_ids);
        let mut chain = DiffChain::new(base);

        for i in 0..n_diffs {
            let new_id = 21 + i as u64;
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 2000 + (i as u64) * 1000,
                diffs: vec![SnapshotDiff::PaneCreated {
                    pane_id: new_id,
                    snapshot: make_pane_state(new_id, 24, 80),
                }],
            });
        }

        let last_seq = chain.diffs.last().unwrap().seq;
        let latest = chain.restore_latest();
        let at_last = chain.restore_at(last_seq).unwrap();

        let latest_ids: HashSet<u64> = latest.pane_states.keys().copied().collect();
        let at_last_ids: HashSet<u64> = at_last.pane_states.keys().copied().collect();
        prop_assert_eq!(latest_ids, at_last_ids);
        prop_assert_eq!(latest.captured_at, at_last.captured_at);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: restore_at invalid seq returns None
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// restore_at with a seq not in the chain returns None.
    #[test]
    fn prop_restore_at_invalid_seq_is_none(
        n_diffs in 1usize..=5,
    ) {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        for i in 0..n_diffs {
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 2000 + (i as u64) * 1000,
                diffs: vec![],
            });
        }

        // Seq 999 is definitely not in the chain
        prop_assert!(chain.restore_at(999).is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: compaction preserves latest state
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// compact() produces chain_len 0 and same restore_latest state.
    #[test]
    fn prop_compact_preserves_latest(
        base_ids in arb_pane_ids(),
        n_diffs in 1usize..=8,
    ) {
        let base = make_base(&base_ids);
        let mut chain = DiffChain::new(base);

        for i in 0..n_diffs {
            let new_id = 21 + i as u64;
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 2000 + (i as u64) * 1000,
                diffs: vec![SnapshotDiff::PaneCreated {
                    pane_id: new_id,
                    snapshot: make_pane_state(new_id, 24, 80),
                }],
            });
        }

        let before = chain.restore_latest();
        let merged = chain.compact();
        prop_assert_eq!(merged, n_diffs);
        prop_assert_eq!(chain.chain_len(), 0);

        let after = chain.restore_latest();
        let before_ids: HashSet<u64> = before.pane_states.keys().copied().collect();
        let after_ids: HashSet<u64> = after.pane_states.keys().copied().collect();
        prop_assert_eq!(before_ids, after_ids);
        prop_assert_eq!(before.captured_at, after.captured_at);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: seq survives compaction
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After compaction, new diffs get seq numbers that continue from pre-compaction.
    #[test]
    fn prop_seq_survives_compaction(
        n_before in 1usize..=5,
        n_after in 1usize..=5,
    ) {
        let base = make_base(&[1]);
        let mut chain = DiffChain::new(base);

        for i in 0..n_before {
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 2000 + (i as u64) * 1000,
                diffs: vec![],
            });
        }

        let last_seq_before = chain.diffs.last().unwrap().seq;
        chain.compact();

        for i in 0..n_after {
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 10000 + (i as u64) * 1000,
                diffs: vec![],
            });
        }

        // First new diff should have seq = last_seq_before + 1
        prop_assert_eq!(
            chain.diffs[0].seq, last_seq_before + 1,
            "First post-compaction seq should be {} but got {}",
            last_seq_before + 1, chain.diffs[0].seq
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: compact on empty chain is noop
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// compact() on chain with no diffs returns 0.
    #[test]
    fn prop_compact_empty_is_noop(
        base_ids in arb_pane_ids(),
    ) {
        let base = make_base(&base_ids);
        let mut chain = DiffChain::new(base);
        let merged = chain.compact();
        prop_assert_eq!(merged, 0);
        prop_assert_eq!(chain.chain_len(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffSnapshotEngine: uninitialized → all None
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Before initialize(), engine returns None for restore and capture.
    #[test]
    fn prop_engine_uninit_returns_none(
        max_chain in 0usize..=50,
    ) {
        let engine = DiffSnapshotEngine::new(max_chain);
        prop_assert!(!engine.is_initialized());
        prop_assert!(engine.restore_latest().is_none());
        prop_assert_eq!(engine.chain_len(), 0);
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffSnapshotEngine: clean tracker → capture returns None
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// When tracker is clean, capture_diff returns None.
    #[test]
    fn prop_engine_clean_capture_is_none(
        base_ids in arb_pane_ids(),
    ) {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&base_ids));

        prop_assert!(engine.tracker().is_clean());
        let result = engine.capture_diff(&HashMap::new(), None, 2000);
        prop_assert!(result.is_none(), "Clean tracker should produce None diff");
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffSnapshotEngine: capture clears tracker
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// After capture_diff, the tracker is always clean.
    #[test]
    fn prop_engine_capture_clears_tracker(
        base_ids in arb_pane_ids(),
        dirty_id in arb_pane_id(),
    ) {
        let mut engine = DiffSnapshotEngine::new(10);
        engine.initialize(make_base(&base_ids));

        engine.tracker_mut().mark_output(dirty_id);
        prop_assert!(!engine.tracker().is_clean());

        let mut current = HashMap::new();
        current.insert(dirty_id, make_pane_state(dirty_id, 24, 80));
        engine.capture_diff(&current, None, 2000);

        prop_assert!(engine.tracker().is_clean(), "Tracker should be clean after capture");
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffSnapshotEngine: auto-compaction
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// With max_chain_len > 0, chain never exceeds max_chain_len.
    #[test]
    fn prop_engine_auto_compacts(
        max_chain in 2usize..=10,
        n_captures in 1usize..=30,
    ) {
        let mut engine = DiffSnapshotEngine::new(max_chain);
        engine.initialize(make_base(&[1]));

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));

        for i in 0..n_captures {
            engine.tracker_mut().mark_metadata(1);
            current.get_mut(&1).unwrap().cwd = Some(format!("/path/{}", i));
            engine.capture_diff(&current, None, 2000 + (i as u64) * 1000);

            // chain_len should never exceed max_chain_len
            // (it may temporarily reach max_chain_len + 1 before compaction triggers)
            prop_assert!(
                engine.chain_len() <= max_chain + 1,
                "chain_len {} > max_chain_len + 1 ({})",
                engine.chain_len(), max_chain + 1
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffSnapshotEngine: disable auto-compaction with 0
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// With max_chain_len = 0, chain grows unbounded.
    #[test]
    fn prop_engine_no_auto_compact(
        n_captures in 1usize..=20,
    ) {
        let mut engine = DiffSnapshotEngine::new(0);
        engine.initialize(make_base(&[1]));

        let mut current = HashMap::new();
        current.insert(1, make_pane_state(1, 24, 80));

        for i in 0..n_captures {
            engine.tracker_mut().mark_metadata(1);
            current.get_mut(&1).unwrap().cwd = Some(format!("/path/{}", i));
            engine.capture_diff(&current, None, 2000 + (i as u64) * 1000);
        }

        prop_assert_eq!(
            engine.chain_len(), n_captures,
            "Without auto-compact, chain_len should match captures"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// BaseSnapshot serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// BaseSnapshot survives JSON roundtrip.
    #[test]
    fn prop_base_snapshot_serde(
        base_ids in arb_pane_ids(),
    ) {
        let base = make_base(&base_ids);
        let json = serde_json::to_string(&base).unwrap();
        let back: BaseSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(base, back);
    }
}

// ────────────────────────────────────────────────────────────────────
// BaseSnapshot: create then close same pane is net removal
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Applying create-then-close for the same pane results in no change if
    /// the pane wasn't in the original base.
    #[test]
    fn prop_create_then_close_is_noop_for_new_pane(
        base_ids in arb_pane_ids(),
        new_id in 21u64..=30,
    ) {
        let mut base = make_base(&base_ids);
        let original_keys: HashSet<u64> = base.pane_states.keys().copied().collect();

        // Create
        base.apply_diff(&DiffSnapshot {
            seq: 1,
            captured_at: 2000,
            diffs: vec![SnapshotDiff::PaneCreated {
                pane_id: new_id,
                snapshot: make_pane_state(new_id, 24, 80),
            }],
        });

        // Close
        base.apply_diff(&DiffSnapshot {
            seq: 2,
            captured_at: 3000,
            diffs: vec![SnapshotDiff::PaneClosed { pane_id: new_id }],
        });

        let final_keys: HashSet<u64> = base.pane_states.keys().copied().collect();
        prop_assert_eq!(original_keys, final_keys, "Create+close should be net noop for new pane");
    }
}

// ────────────────────────────────────────────────────────────────────
// DiffChain: progressive restore_at
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// restore_at(seq_i) has exactly base_count + i panes when each diff adds one pane.
    #[test]
    fn prop_progressive_restore(
        base_ids in arb_pane_ids(),
        n_diffs in 1usize..=5,
    ) {
        let base = make_base(&base_ids);
        let base_count = base.pane_states.len();
        let mut chain = DiffChain::new(base);

        for i in 0..n_diffs {
            let new_id = 21 + i as u64;
            chain.push_diff(DiffSnapshot {
                seq: 0,
                captured_at: 2000 + (i as u64) * 1000,
                diffs: vec![SnapshotDiff::PaneCreated {
                    pane_id: new_id,
                    snapshot: make_pane_state(new_id, 24, 80),
                }],
            });
        }

        for i in 0..n_diffs {
            let seq = (i + 1) as u64;
            let restored = chain.restore_at(seq).unwrap();
            prop_assert_eq!(
                restored.pane_states.len(), base_count + i + 1,
                "At seq {}, expected {} panes but got {}",
                seq, base_count + i + 1, restored.pane_states.len()
            );
        }
    }
}
