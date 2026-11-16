//! Property-based tests for `frankenterm_core::screen_state` module.
//!
//! Validates:
//! 1.  New tracker — is_alt_screen returns false for any pane_id
//! 2.  Enter 1049 — process_output with \x1b[?1049h sets alt_screen=true
//! 3.  Leave 1049 — after enter, \x1b[?1049l sets alt_screen=false
//! 4.  Enter 47 — \x1b[?47h sets alt_screen=true
//! 5.  Leave 47 — \x1b[?47l sets alt_screen=false
//! 6.  Mixed enter 1049, leave 47
//! 7.  Mixed enter 47, leave 1049
//! 8.  Sequence in middle of output
//! 9.  Multiple sequences in single output — last one wins
//! 10. Last-wins invariant (PROPERTY)
//! 11. Pane independence (PROPERTY)
//! 12. Empty output doesn't change state
//! 13. Clear pane removes state
//! 14. Set alt_screen directly true then false
//! 15. Set alt_screen directly matches query
//! 16. Reset context clears tail buffer
//! 17. Tracked panes grows with new pane_ids
//! 18. Tracked panes shrinks with clear_pane
//! 19. Split sequence across captures (PROPERTY)
//! 20. Split 47 sequence across captures (PROPERTY)
//! 21. Random bytes without ESC never changes state
//! 22. Other ESC sequences don't change state
//! 23. Idempotent enter — entering alt-screen twice keeps it true
//! 24. Idempotent leave — leaving alt-screen twice keeps it false
//! 25. Determinism — same operations always produce same state
//! 26. Sequence exact byte patterns
//! 27. Enter then clear then enter — pane restarts cleanly
//! 28. Multiple panes enter/leave independently
//! 29. Large random output without sequences
//! 30. Noise prefix + enter sequence
//! 31. Enter sequence + noise suffix
//! 32. Noise + enter + noise + leave + noise
//! 33. set_alt_screen then process_output overrides
//! 34. process_output then set_alt_screen overrides
//! 35. clear_pane on unknown pane is no-op
//! 36. reset_context on unknown pane is no-op
//! 37. is_alt_screen on cleared pane returns false
//! 38. tracked_panes after clear_pane removes exactly the cleared pane
//! 39. Enter/leave sequence operations are order-dependent
//! 40. Split leave sequence across captures
//! 41. Interleaved multi-pane operations
//! 42. Multiple enter sequences in one buffer — last one still yields true
//! 43. Many enters then one leave yields false
//! 44. Empty output does not create pane entry (early return)
//! 45. Byte-level verification of all four escape sequences
//! 46. Back-to-back enter/leave sequences
//! 47. Stress: random operation sequence on multiple panes

use proptest::prelude::*;

use frankenterm_core::screen_state::ScreenStateTracker;

// =============================================================================
// Constants — escape sequence bytes (mirrors source)
// =============================================================================

const ENTER_1049: &[u8] = b"\x1b[?1049h";
const LEAVE_1049: &[u8] = b"\x1b[?1049l";
const ENTER_47: &[u8] = b"\x1b[?47h";
const LEAVE_47: &[u8] = b"\x1b[?47l";

// =============================================================================
// Strategies
// =============================================================================

/// A pane id in a small range so collisions are testable.
fn arb_pane_id() -> impl Strategy<Value = u64> {
    0u64..100
}

/// A pane id from the full u64 range.
fn arb_pane_id_wide() -> impl Strategy<Value = u64> {
    any::<u64>()
}

/// Random bytes that do NOT contain any ESC (0x1b) byte.
fn arb_non_esc_bytes(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(0x00u8..0x1bu8, 0..max_len).prop_map(|mut v| {
        // Also filter out accidental 0x1b from the range boundary.
        v.retain(|&b| b != 0x1b);
        v
    })
}

/// One of the four recognised escape sequences.
fn arb_enter_seq() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![Just(ENTER_1049.to_vec()), Just(ENTER_47.to_vec()),]
}

fn arb_leave_seq() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![Just(LEAVE_1049.to_vec()), Just(LEAVE_47.to_vec()),]
}

/// An enter or leave operation (true = enter, false = leave).
fn arb_screen_op() -> impl Strategy<Value = (bool, Vec<u8>)> {
    prop_oneof![
        Just((true, ENTER_1049.to_vec())),
        Just((true, ENTER_47.to_vec())),
        Just((false, LEAVE_1049.to_vec())),
        Just((false, LEAVE_47.to_vec())),
    ]
}

/// Generate a sequence of enter/leave operations (1..max_ops).
fn arb_ops(max_ops: usize) -> impl Strategy<Value = Vec<(bool, Vec<u8>)>> {
    prop::collection::vec(arb_screen_op(), 1..max_ops)
}

/// Non-alt-screen ESC sequences that should be ignored.
fn arb_other_esc_seq() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        Just(b"\x1b[2J".to_vec()),     // clear screen
        Just(b"\x1b[H".to_vec()),      // cursor home
        Just(b"\x1b[0m".to_vec()),     // reset attrs
        Just(b"\x1b[32m".to_vec()),    // green foreground
        Just(b"\x1b[1;1H".to_vec()),   // cursor to 1,1
        Just(b"\x1b[?25h".to_vec()),   // show cursor
        Just(b"\x1b[?25l".to_vec()),   // hide cursor
        Just(b"\x1b[K".to_vec()),      // erase to end of line
        Just(b"\x1b[?1000h".to_vec()), // enable mouse
        Just(b"\x1b[?2004h".to_vec()), // enable bracketed paste
    ]
}

// =============================================================================
// Tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── 1. New tracker returns false for any pane ──────────────────────
    #[test]
    fn new_tracker_is_alt_screen_false(pane_id in arb_pane_id_wide()) {
        let tracker = ScreenStateTracker::new();
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "new tracker should return false for pane {}", pane_id);
    }

    // ── 2. Enter 1049 sets alt_screen=true ─────────────────────────────
    #[test]
    fn enter_1049_sets_true(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_1049);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "after ENTER_1049 pane {} should be alt_screen", pane_id);
    }

    // ── 3. Leave 1049 sets alt_screen=false ────────────────────────────
    #[test]
    fn leave_1049_after_enter(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_1049);
        tracker.process_output(pane_id, LEAVE_1049);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "after ENTER then LEAVE pane {} should not be alt_screen", pane_id);
    }

    // ── 4. Enter 47 sets alt_screen=true ───────────────────────────────
    #[test]
    fn enter_47_sets_true(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_47);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "after ENTER_47 pane {} should be alt_screen", pane_id);
    }

    // ── 5. Leave 47 sets alt_screen=false ──────────────────────────────
    #[test]
    fn leave_47_after_enter(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_47);
        tracker.process_output(pane_id, LEAVE_47);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "after ENTER_47 then LEAVE_47 pane {} should not be alt_screen", pane_id);
    }

    // ── 6. Mixed: enter 1049, leave 47 ────────────────────────────────
    #[test]
    fn mixed_enter_1049_leave_47(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_1049);
        prop_assert!(tracker.is_alt_screen(pane_id));
        tracker.process_output(pane_id, LEAVE_47);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "enter 1049 then leave 47 should yield false for pane {}", pane_id);
    }

    // ── 7. Mixed: enter 47, leave 1049 ────────────────────────────────
    #[test]
    fn mixed_enter_47_leave_1049(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_47);
        prop_assert!(tracker.is_alt_screen(pane_id));
        tracker.process_output(pane_id, LEAVE_1049);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "enter 47 then leave 1049 should yield false for pane {}", pane_id);
    }

    // ── 8. Sequence in middle of output ────────────────────────────────
    #[test]
    fn enter_in_middle_of_output(
        prefix in arb_non_esc_bytes(50),
        suffix in arb_non_esc_bytes(50),
        pane_id in arb_pane_id_wide(),
        enter_seq in arb_enter_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut buf = prefix;
        buf.extend_from_slice(&enter_seq);
        buf.extend_from_slice(&suffix);
        tracker.process_output(pane_id, &buf);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "enter sequence embedded in noise should set alt_screen for pane {}", pane_id);
    }

    // ── 9. Multiple sequences — last one wins (enter,leave -> false) ──
    #[test]
    fn multiple_seqs_last_wins_false(
        pane_id in arb_pane_id_wide(),
        enter_seq in arb_enter_seq(),
        leave_seq in arb_leave_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut buf = Vec::new();
        buf.extend_from_slice(&enter_seq);
        buf.extend_from_slice(b"some content");
        buf.extend_from_slice(&leave_seq);
        tracker.process_output(pane_id, &buf);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "enter then leave in single buffer should be false for pane {}", pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── 10. Last-wins invariant (PROPERTY) ─────────────────────────────
    #[test]
    fn last_wins_invariant(
        ops in arb_ops(20),
        pane_id in arb_pane_id_wide(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut expected = false;
        for (is_enter, seq) in &ops {
            tracker.process_output(pane_id, seq);
            expected = *is_enter;
        }
        prop_assert_eq!(tracker.is_alt_screen(pane_id), expected,
            "final state should match last op for pane {}", pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── 11. Pane independence (PROPERTY) ───────────────────────────────
    #[test]
    fn pane_independence(
        pane_a in 0u64..50,
        pane_b in 50u64..100,
        enter_seq in arb_enter_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_a, &enter_seq);
        tracker.process_output(pane_b, b"normal output");
        prop_assert!(tracker.is_alt_screen(pane_a),
            "pane_a {} should be alt_screen", pane_a);
        prop_assert!(!tracker.is_alt_screen(pane_b),
            "pane_b {} should NOT be alt_screen", pane_b);
    }

    // ── 12. Empty output doesn't change state ─────────────────────────
    #[test]
    fn empty_output_preserves_state(
        pane_id in arb_pane_id_wide(),
        enter in prop::bool::ANY,
    ) {
        let mut tracker = ScreenStateTracker::new();
        if enter {
            tracker.process_output(pane_id, ENTER_1049);
        }
        let before = tracker.is_alt_screen(pane_id);
        tracker.process_output(pane_id, b"");
        prop_assert_eq!(tracker.is_alt_screen(pane_id), before,
            "empty output should not change state for pane {}", pane_id);
    }

    // ── 13. Clear pane removes state ──────────────────────────────────
    #[test]
    fn clear_pane_removes_state(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_1049);
        prop_assert!(tracker.is_alt_screen(pane_id));
        tracker.clear_pane(pane_id);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "after clear_pane, pane {} should not be alt_screen", pane_id);
    }

    // ── 14. Set alt_screen directly true then false ───────────────────
    #[test]
    fn set_alt_screen_true_then_false(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.set_alt_screen(pane_id, true);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "set_alt_screen(true) should be queryable for pane {}", pane_id);
        tracker.set_alt_screen(pane_id, false);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "set_alt_screen(false) should be queryable for pane {}", pane_id);
    }

    // ── 15. Set alt_screen matches query for any bool ─────────────────
    #[test]
    fn set_alt_screen_matches_query(
        pane_id in arb_pane_id_wide(),
        active in prop::bool::ANY,
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.set_alt_screen(pane_id, active);
        prop_assert_eq!(tracker.is_alt_screen(pane_id), active,
            "set_alt_screen({}) should match query for pane {}", active, pane_id);
    }

    // ── 16. Reset context clears tail buffer ──────────────────────────
    #[test]
    fn reset_context_breaks_split_sequence(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        // Start a split 1049 enter sequence: send partial
        tracker.process_output(pane_id, b"text\x1b[?10");
        tracker.reset_context(pane_id);
        // Send the remainder — should NOT complete the sequence
        tracker.process_output(pane_id, b"49h");
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "after reset_context, split seq should not complete for pane {}", pane_id);
    }

    // ── 17. Tracked panes grows with new pane_ids ─────────────────────
    #[test]
    fn tracked_panes_grows(
        pane_ids in prop::collection::hash_set(arb_pane_id(), 1..20),
    ) {
        let mut tracker = ScreenStateTracker::new();
        for &pid in &pane_ids {
            tracker.process_output(pid, b"some output");
        }
        let tracked = tracker.tracked_panes();
        prop_assert_eq!(tracked.len(), pane_ids.len(),
            "tracked panes count {} should match inserted count {}", tracked.len(), pane_ids.len());
        for &pid in &pane_ids {
            let contains = tracked.contains(&pid);
            prop_assert!(contains, "tracked panes should contain pane {}", pid);
        }
    }

    // ── 18. Tracked panes shrinks with clear_pane ─────────────────────
    #[test]
    fn tracked_panes_shrinks_on_clear(
        pane_ids in prop::collection::vec(arb_pane_id(), 2..10),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut unique: Vec<u64> = pane_ids.clone();
        unique.sort();
        unique.dedup();

        for &pid in &unique {
            tracker.process_output(pid, b"output");
        }
        prop_assert_eq!(tracker.tracked_panes().len(), unique.len());

        // Clear the first pane
        let removed = unique[0];
        tracker.clear_pane(removed);
        let tracked = tracker.tracked_panes();
        let contains_removed = tracked.contains(&removed);
        prop_assert!(!contains_removed,
            "tracked panes should not contain cleared pane {}", removed);
        prop_assert_eq!(tracked.len(), unique.len() - 1,
            "tracked panes count should shrink by 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // ── 19. Split 1049 enter sequence across captures (PROPERTY) ──────
    #[test]
    fn split_1049_enter_across_captures(
        pane_id in arb_pane_id_wide(),
        split_point in 1usize..7,
    ) {
        // ENTER_1049 is 8 bytes: \x1b[?1049h
        let seq = ENTER_1049;
        let sp = split_point.min(seq.len() - 1);
        let (part1, part2) = seq.split_at(sp);

        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, part1);
        // After the first partial chunk, the sequence should not yet be detected
        // (unless part1 itself is the full sequence, which we prevent via sp < len).
        tracker.process_output(pane_id, part2);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "split ENTER_1049 at byte {} should be detected for pane {}", sp, pane_id);
    }

    // ── 20. Split 47 enter sequence across captures (PROPERTY) ────────
    #[test]
    fn split_47_enter_across_captures(
        pane_id in arb_pane_id_wide(),
        split_point in 1usize..5,
    ) {
        // ENTER_47 is 6 bytes: \x1b[?47h
        let seq = ENTER_47;
        let sp = split_point.min(seq.len() - 1);
        let (part1, part2) = seq.split_at(sp);

        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, part1);
        tracker.process_output(pane_id, part2);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "split ENTER_47 at byte {} should be detected for pane {}", sp, pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── 21. Random bytes without ESC never changes state ──────────────
    #[test]
    fn random_non_esc_bytes_no_state_change(
        noise in arb_non_esc_bytes(200),
        pane_id in arb_pane_id_wide(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, &noise);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "non-ESC bytes should never set alt_screen for pane {}", pane_id);
    }

    // ── 22. Other ESC sequences don't change state ────────────────────
    #[test]
    fn other_esc_sequences_ignored(
        seqs in prop::collection::vec(arb_other_esc_seq(), 1..10),
        pane_id in arb_pane_id_wide(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let buf: Vec<u8> = seqs.into_iter().flatten().collect();
        tracker.process_output(pane_id, &buf);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "non-alt-screen ESC sequences should not set alt_screen for pane {}", pane_id);
    }

    // ── 23. Idempotent enter — entering twice keeps true ──────────────
    #[test]
    fn idempotent_enter(
        pane_id in arb_pane_id_wide(),
        enter_seq in arb_enter_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, &enter_seq);
        prop_assert!(tracker.is_alt_screen(pane_id));
        tracker.process_output(pane_id, &enter_seq);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "double enter should still be true for pane {}", pane_id);
    }

    // ── 24. Idempotent leave — leaving twice keeps false ──────────────
    #[test]
    fn idempotent_leave(
        pane_id in arb_pane_id_wide(),
        leave_seq in arb_leave_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, &leave_seq);
        prop_assert!(!tracker.is_alt_screen(pane_id));
        tracker.process_output(pane_id, &leave_seq);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "double leave should still be false for pane {}", pane_id);
    }

    // ── 25. Determinism — same operations produce same state ──────────
    #[test]
    fn determinism(
        ops in arb_ops(15),
        pane_id in arb_pane_id_wide(),
    ) {
        let mut tracker1 = ScreenStateTracker::new();
        let mut tracker2 = ScreenStateTracker::new();
        for (_, seq) in &ops {
            tracker1.process_output(pane_id, seq);
            tracker2.process_output(pane_id, seq);
        }
        prop_assert_eq!(
            tracker1.is_alt_screen(pane_id),
            tracker2.is_alt_screen(pane_id),
            "same ops on two trackers should yield same state for pane {}", pane_id
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── 26. Exact byte patterns ───────────────────────────────────────
    #[test]
    fn exact_byte_patterns(_dummy in Just(())) {
        // Verify the exact byte patterns for all four sequences
        prop_assert_eq!(ENTER_1049, b"\x1b[?1049h".as_slice(),
            "ENTER_1049 bytes mismatch");
        prop_assert_eq!(LEAVE_1049, b"\x1b[?1049l".as_slice(),
            "LEAVE_1049 bytes mismatch");
        prop_assert_eq!(ENTER_47, b"\x1b[?47h".as_slice(),
            "ENTER_47 bytes mismatch");
        prop_assert_eq!(LEAVE_47, b"\x1b[?47l".as_slice(),
            "LEAVE_47 bytes mismatch");

        // Verify lengths
        prop_assert_eq!(ENTER_1049.len(), 8, "ENTER_1049 should be 8 bytes");
        prop_assert_eq!(LEAVE_1049.len(), 8, "LEAVE_1049 should be 8 bytes");
        prop_assert_eq!(ENTER_47.len(), 6, "ENTER_47 should be 6 bytes");
        prop_assert_eq!(LEAVE_47.len(), 6, "LEAVE_47 should be 6 bytes");

        // Verify they start with ESC
        prop_assert_eq!(ENTER_1049[0], 0x1b, "ENTER_1049 should start with ESC");
        prop_assert_eq!(LEAVE_1049[0], 0x1b, "LEAVE_1049 should start with ESC");
        prop_assert_eq!(ENTER_47[0], 0x1b, "ENTER_47 should start with ESC");
        prop_assert_eq!(LEAVE_47[0], 0x1b, "LEAVE_47 should start with ESC");

        // Enter sequences end with 'h', leave with 'l'
        prop_assert_eq!(*ENTER_1049.last().unwrap(), b'h',
            "ENTER_1049 should end with 'h'");
        prop_assert_eq!(*LEAVE_1049.last().unwrap(), b'l',
            "LEAVE_1049 should end with 'l'");
        prop_assert_eq!(*ENTER_47.last().unwrap(), b'h',
            "ENTER_47 should end with 'h'");
        prop_assert_eq!(*LEAVE_47.last().unwrap(), b'l',
            "LEAVE_47 should end with 'l'");
    }

    // ── 27. Enter, clear, re-enter — pane restarts cleanly ────────────
    #[test]
    fn enter_clear_reenter(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_1049);
        prop_assert!(tracker.is_alt_screen(pane_id));

        tracker.clear_pane(pane_id);
        prop_assert!(!tracker.is_alt_screen(pane_id));

        tracker.process_output(pane_id, ENTER_47);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "re-entering after clear should work for pane {}", pane_id);
    }

    // ── 28. Multiple panes enter/leave independently ──────────────────
    #[test]
    fn multi_pane_independent_ops(
        pane_a in 0u64..50,
        pane_b in 50u64..100,
    ) {
        let mut tracker = ScreenStateTracker::new();

        // Both enter
        tracker.process_output(pane_a, ENTER_1049);
        tracker.process_output(pane_b, ENTER_47);
        prop_assert!(tracker.is_alt_screen(pane_a));
        prop_assert!(tracker.is_alt_screen(pane_b));

        // Only pane_a leaves
        tracker.process_output(pane_a, LEAVE_1049);
        prop_assert!(!tracker.is_alt_screen(pane_a),
            "pane_a {} should have left alt_screen", pane_a);
        prop_assert!(tracker.is_alt_screen(pane_b),
            "pane_b {} should still be in alt_screen", pane_b);
    }

    // ── 29. Large random output without sequences ─────────────────────
    #[test]
    fn large_random_output_no_state_change(
        noise in arb_non_esc_bytes(1000),
        pane_id in arb_pane_id_wide(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, &noise);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "large non-ESC output should not alter state for pane {}", pane_id);
    }

    // ── 30. Noise prefix + enter sequence ─────────────────────────────
    #[test]
    fn noise_prefix_then_enter(
        prefix in arb_non_esc_bytes(100),
        pane_id in arb_pane_id_wide(),
        enter_seq in arb_enter_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut buf = prefix;
        buf.extend_from_slice(&enter_seq);
        tracker.process_output(pane_id, &buf);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "noise prefix + enter should set alt_screen for pane {}", pane_id);
    }

    // ── 31. Enter sequence + noise suffix ─────────────────────────────
    #[test]
    fn enter_then_noise_suffix(
        suffix in arb_non_esc_bytes(100),
        pane_id in arb_pane_id_wide(),
        enter_seq in arb_enter_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut buf = enter_seq;
        buf.extend_from_slice(&suffix);
        tracker.process_output(pane_id, &buf);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "enter + noise suffix should set alt_screen for pane {}", pane_id);
    }

    // ── 32. Noise + enter + noise + leave + noise ─────────────────────
    #[test]
    fn noise_enter_noise_leave_noise(
        n1 in arb_non_esc_bytes(30),
        n2 in arb_non_esc_bytes(30),
        n3 in arb_non_esc_bytes(30),
        pane_id in arb_pane_id_wide(),
        enter_seq in arb_enter_seq(),
        leave_seq in arb_leave_seq(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut buf = n1;
        buf.extend_from_slice(&enter_seq);
        buf.extend_from_slice(&n2);
        buf.extend_from_slice(&leave_seq);
        buf.extend_from_slice(&n3);
        tracker.process_output(pane_id, &buf);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "enter then leave with noise around should be false for pane {}", pane_id);
    }

    // ── 33. set_alt_screen then process_output overrides ──────────────
    #[test]
    fn set_then_process_overrides(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.set_alt_screen(pane_id, true);
        prop_assert!(tracker.is_alt_screen(pane_id));
        tracker.process_output(pane_id, LEAVE_1049);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "process_output should override set_alt_screen for pane {}", pane_id);
    }

    // ── 34. process_output then set_alt_screen overrides ──────────────
    #[test]
    fn process_then_set_overrides(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, ENTER_1049);
        prop_assert!(tracker.is_alt_screen(pane_id));
        tracker.set_alt_screen(pane_id, false);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "set_alt_screen should override process_output for pane {}", pane_id);
    }

    // ── 35. clear_pane on unknown pane is no-op ───────────────────────
    #[test]
    fn clear_unknown_pane_noop(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        // Should not panic or error
        tracker.clear_pane(pane_id);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "clear_pane on unknown pane should be a no-op for pane {}", pane_id);
    }

    // ── 36. reset_context on unknown pane is no-op ────────────────────
    #[test]
    fn reset_context_unknown_pane_noop(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        // Should not panic or error
        tracker.reset_context(pane_id);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "reset_context on unknown pane should be a no-op for pane {}", pane_id);
    }

    // ── 37. is_alt_screen on cleared pane returns false ───────────────
    #[test]
    fn cleared_pane_returns_false(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.set_alt_screen(pane_id, true);
        tracker.clear_pane(pane_id);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "cleared pane {} should return false from is_alt_screen", pane_id);
        // Also verify it's no longer tracked
        let tracked = tracker.tracked_panes();
        let still_tracked = tracked.contains(&pane_id);
        prop_assert!(!still_tracked,
            "cleared pane {} should not be in tracked_panes", pane_id);
    }

    // ── 38. tracked_panes after clear removes exactly that pane ───────
    #[test]
    fn tracked_panes_after_clear_exact(
        pane_a in 0u64..50,
        pane_b in 50u64..100,
        pane_c in 100u64..150,
    ) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_a, b"a");
        tracker.process_output(pane_b, b"b");
        tracker.process_output(pane_c, b"c");
        prop_assert_eq!(tracker.tracked_panes().len(), 3);

        tracker.clear_pane(pane_b);
        let tracked = tracker.tracked_panes();
        prop_assert_eq!(tracked.len(), 2, "should have 2 panes after clearing one");
        let has_a = tracked.contains(&pane_a);
        let has_b = tracked.contains(&pane_b);
        let has_c = tracked.contains(&pane_c);
        prop_assert!(has_a, "pane_a {} should remain", pane_a);
        prop_assert!(!has_b, "pane_b {} should be removed", pane_b);
        prop_assert!(has_c, "pane_c {} should remain", pane_c);
    }

    // ── 39. Enter/leave operations are order-dependent ────────────────
    #[test]
    fn order_dependent(pane_id in arb_pane_id_wide()) {
        // Enter then leave => false
        let mut t1 = ScreenStateTracker::new();
        t1.process_output(pane_id, ENTER_1049);
        t1.process_output(pane_id, LEAVE_1049);
        prop_assert!(!t1.is_alt_screen(pane_id),
            "enter then leave should be false for pane {}", pane_id);

        // Leave then enter => true
        let mut t2 = ScreenStateTracker::new();
        t2.process_output(pane_id, LEAVE_1049);
        t2.process_output(pane_id, ENTER_1049);
        prop_assert!(t2.is_alt_screen(pane_id),
            "leave then enter should be true for pane {}", pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // ── 40. Split leave sequence across captures ──────────────────────
    #[test]
    fn split_leave_1049_across_captures(
        pane_id in arb_pane_id_wide(),
        split_point in 1usize..7,
    ) {
        let seq = LEAVE_1049;
        let sp = split_point.min(seq.len() - 1);
        let (part1, part2) = seq.split_at(sp);

        let mut tracker = ScreenStateTracker::new();
        // First enter so we can detect the leave
        tracker.process_output(pane_id, ENTER_1049);
        prop_assert!(tracker.is_alt_screen(pane_id));

        // Now split the leave across two captures
        tracker.process_output(pane_id, part1);
        tracker.process_output(pane_id, part2);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "split LEAVE_1049 at byte {} should be detected for pane {}", sp, pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ── 41. Interleaved multi-pane operations ─────────────────────────
    #[test]
    fn interleaved_multi_pane(
        ops_a in arb_ops(10),
        ops_b in arb_ops(10),
    ) {
        let pane_a: u64 = 1;
        let pane_b: u64 = 2;
        let mut tracker = ScreenStateTracker::new();

        let mut expected_a = false;
        let mut expected_b = false;

        // Interleave operations from both panes
        let max_len = ops_a.len().max(ops_b.len());
        for i in 0..max_len {
            if let Some((is_enter, seq)) = ops_a.get(i) {
                tracker.process_output(pane_a, seq);
                expected_a = *is_enter;
            }
            if let Some((is_enter, seq)) = ops_b.get(i) {
                tracker.process_output(pane_b, seq);
                expected_b = *is_enter;
            }
        }

        prop_assert_eq!(tracker.is_alt_screen(pane_a), expected_a,
            "pane_a final state mismatch");
        prop_assert_eq!(tracker.is_alt_screen(pane_b), expected_b,
            "pane_b final state mismatch");
    }

    // ── 42. Multiple enter sequences in one buffer — still true ───────
    #[test]
    fn multiple_enters_still_true(
        pane_id in arb_pane_id_wide(),
        count in 2usize..10,
    ) {
        let mut tracker = ScreenStateTracker::new();
        let buf: Vec<u8> = ENTER_1049.repeat(count);
        tracker.process_output(pane_id, &buf);
        prop_assert!(tracker.is_alt_screen(pane_id),
            "{} enter sequences should still yield true for pane {}", count, pane_id);
    }

    // ── 43. Many enters then one leave yields false ───────────────────
    #[test]
    fn many_enters_one_leave(
        pane_id in arb_pane_id_wide(),
        enter_count in 2usize..10,
    ) {
        let mut tracker = ScreenStateTracker::new();
        let mut buf: Vec<u8> = ENTER_1049.repeat(enter_count);
        buf.extend_from_slice(LEAVE_1049);
        tracker.process_output(pane_id, &buf);
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "{} enters then leave should be false for pane {}", enter_count, pane_id);
    }

    // ── 44. Empty output does not create pane entry ───────────────────
    #[test]
    fn empty_output_no_pane_entry(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();
        tracker.process_output(pane_id, b"");
        // The source has early return for empty output, so pane should NOT be tracked
        let tracked = tracker.tracked_panes();
        let is_tracked = tracked.contains(&pane_id);
        prop_assert!(!is_tracked,
            "empty output should not create pane entry for pane {}", pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── 45. Byte-level verification of all four sequences ─────────────
    #[test]
    fn byte_level_all_four_sequences(pane_id in arb_pane_id_wide()) {
        let mut tracker = ScreenStateTracker::new();

        // Test each of the 4 sequences independently
        // 1049 enter
        tracker.process_output(pane_id, &[0x1b, b'[', b'?', b'1', b'0', b'4', b'9', b'h']);
        prop_assert!(tracker.is_alt_screen(pane_id), "1049 enter byte-level");

        // 1049 leave
        tracker.process_output(pane_id, &[0x1b, b'[', b'?', b'1', b'0', b'4', b'9', b'l']);
        prop_assert!(!tracker.is_alt_screen(pane_id), "1049 leave byte-level");

        // 47 enter
        tracker.process_output(pane_id, &[0x1b, b'[', b'?', b'4', b'7', b'h']);
        prop_assert!(tracker.is_alt_screen(pane_id), "47 enter byte-level");

        // 47 leave
        tracker.process_output(pane_id, &[0x1b, b'[', b'?', b'4', b'7', b'l']);
        prop_assert!(!tracker.is_alt_screen(pane_id), "47 leave byte-level");
    }

    // ── 46. Back-to-back enter/leave pairs ────────────────────────────
    #[test]
    fn back_to_back_pairs(
        pane_id in arb_pane_id_wide(),
        pairs in 1usize..20,
    ) {
        let mut tracker = ScreenStateTracker::new();
        // Build buffer: (enter + leave) repeated `pairs` times
        let mut buf = Vec::new();
        for _ in 0..pairs {
            buf.extend_from_slice(ENTER_1049);
            buf.extend_from_slice(LEAVE_1049);
        }
        tracker.process_output(pane_id, &buf);
        // Last operation is always leave, so result is false
        prop_assert!(!tracker.is_alt_screen(pane_id),
            "{} enter/leave pairs should end false for pane {}", pairs, pane_id);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── 47. Stress: random operation sequence on multiple panes ───────
    #[test]
    fn stress_random_ops_multi_pane(
        ops in prop::collection::vec(
            (arb_pane_id(), arb_screen_op()),
            1..50
        ),
    ) {
        let mut tracker = ScreenStateTracker::new();
        // Track expected state per pane
        let mut expected: std::collections::HashMap<u64, bool> = std::collections::HashMap::new();

        for (pane_id, (is_enter, seq)) in &ops {
            tracker.process_output(*pane_id, seq);
            expected.insert(*pane_id, *is_enter);
        }

        for (pane_id, exp) in &expected {
            prop_assert_eq!(tracker.is_alt_screen(*pane_id), *exp,
                "stress: pane {} expected {} but got {}", pane_id, exp, tracker.is_alt_screen(*pane_id));
        }
    }
}

// =============================================================================
// Additional property: last-wins within a single buffer (PROPERTY)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // ── 48. Last-wins within a single buffer ──────────────────────────
    #[test]
    fn last_wins_single_buffer(
        ops in arb_ops(15),
        pane_id in arb_pane_id_wide(),
    ) {
        let mut tracker = ScreenStateTracker::new();
        // Concatenate all ops into a single buffer
        let buf: Vec<u8> = ops.iter().flat_map(|(_, seq)| seq.iter().copied()).collect();
        let expected = ops.last().map(|(is_enter, _)| *is_enter).unwrap_or(false);
        tracker.process_output(pane_id, &buf);
        prop_assert_eq!(tracker.is_alt_screen(pane_id), expected,
            "single-buffer last-wins for pane {}", pane_id);
    }

    // ── 49. Equivalence: sequential vs. single-buffer processing ──────
    #[test]
    fn sequential_vs_single_buffer_equivalence(
        ops in arb_ops(10),
        pane_id in arb_pane_id_wide(),
    ) {
        // Sequential processing: one call per operation
        let mut seq_tracker = ScreenStateTracker::new();
        for (_, seq) in &ops {
            seq_tracker.process_output(pane_id, seq);
        }

        // Single-buffer processing: all in one call
        let mut buf_tracker = ScreenStateTracker::new();
        let buf: Vec<u8> = ops.iter().flat_map(|(_, seq)| seq.iter().copied()).collect();
        buf_tracker.process_output(pane_id, &buf);

        prop_assert_eq!(
            seq_tracker.is_alt_screen(pane_id),
            buf_tracker.is_alt_screen(pane_id),
            "sequential vs single-buffer should yield same result for pane {}", pane_id
        );
    }

    // ── 50. set_alt_screen creates tracked pane ───────────────────────
    #[test]
    fn set_alt_screen_creates_pane(
        pane_id in arb_pane_id_wide(),
        active in prop::bool::ANY,
    ) {
        let mut tracker = ScreenStateTracker::new();
        prop_assert!(tracker.tracked_panes().is_empty());
        tracker.set_alt_screen(pane_id, active);
        let tracked = tracker.tracked_panes();
        let is_tracked = tracked.contains(&pane_id);
        prop_assert!(is_tracked,
            "set_alt_screen should cause pane {} to be tracked", pane_id);
    }
}
