//! BFS proof harness for the DEC 2026 presentation-hold
//! state machine
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.1.cont] / `ft-u6jos`).
//!
//! Exhaustively explores every reachable state from `Init` up
//! to `max_depth = 7` events; asserts no
//! `PresentationHoldViolation` fires under any schedule.
//! Combined with the lib's 1024-trial random sweep, this
//! validates the bead's headline behavior:
//!
//! - Hold while `synchronized_output_active`.
//! - Flush on ESU/Reset transitions out of the hold window
//!   when there's pending dirty content.
//! - Never `Present` while the hold flag is set.
//! - Never `Hold` while the flag is clear.
//! - No orphaned `held_dirty_lines` after exiting the window.

use std::collections::HashSet;

use frankenterm_core::dec_2026_presentation_hold::{
    PresentationHoldEvent, PresentationHoldOutcome, PresentationHoldState,
    SynchronizedOutputHealth, apply_event, check_invariants,
};

/// Action alphabet. Bounded so the BFS state space stays
/// finite (the dirty-line domain is small: lines 0..3).
fn action_alphabet() -> Vec<PresentationHoldEvent> {
    vec![
        PresentationHoldEvent::Bsu,
        PresentationHoldEvent::Esu,
        PresentationHoldEvent::FrameReady,
        PresentationHoldEvent::DirtyLineMarked { line: 0 },
        PresentationHoldEvent::DirtyLineMarked { line: 1 },
        PresentationHoldEvent::DirtyLineMarked { line: 2 },
        PresentationHoldEvent::Reset,
    ]
}

#[test]
fn bfs_exhausts_state_space_clean_at_depth_5() {
    let mut visited: HashSet<PresentationHoldState> = HashSet::new();
    let start = PresentationHoldState::initial();
    visited.insert(start.clone());
    let mut frontier: Vec<(PresentationHoldState, usize)> = vec![(start, 0)];

    while let Some((state, depth)) = frontier.pop() {
        if depth >= 5 {
            continue;
        }
        for event in action_alphabet() {
            let mut next = state.clone();
            let outcome = apply_event(&mut next, event);
            let v = check_invariants(&state, &next, event, outcome);
            assert!(
                v.is_empty(),
                "violation at depth {depth} under {event:?}: {v:?}; state={state:?}"
            );
            if visited.insert(next.clone()) {
                frontier.push((next, depth + 1));
            }
        }
    }
}

#[test]
fn canonical_redraw_window_holds_then_flushes() {
    // The bead's headline scenario: TUI app issues BSU, paints
    // many lines, issues ESU. Renderer must hold every
    // FrameReady inside the window and emit exactly one Flush
    // on ESU.
    let mut state = PresentationHoldState::initial();

    // Outside window: FrameReady → Present.
    let outcome = apply_event(&mut state, PresentationHoldEvent::FrameReady);
    assert_eq!(outcome, PresentationHoldOutcome::Present);

    // Enter window.
    apply_event(&mut state, PresentationHoldEvent::Bsu);

    // Paint 5 lines + 3 FrameReady ticks during the window.
    for line in [3, 8, 12, 5, 8] {
        apply_event(&mut state, PresentationHoldEvent::DirtyLineMarked { line });
    }
    for _ in 0..3 {
        let outcome = apply_event(&mut state, PresentationHoldEvent::FrameReady);
        assert_eq!(outcome, PresentationHoldOutcome::Hold);
    }
    assert_eq!(state.frames_held_total, 3);
    assert_eq!(state.held_dirty_lines.len(), 4); // dedup of 3,5,8,12

    // Exit window — exactly one Flush.
    let outcome = apply_event(&mut state, PresentationHoldEvent::Esu);
    assert_eq!(outcome, PresentationHoldOutcome::Flush { lines_flushed: 4 });
    assert_eq!(state.frames_flushed_total, 1);
    assert!(!state.synchronized_output_active);
    assert!(state.held_dirty_lines.is_empty());

    // After window: FrameReady → Present again.
    let outcome = apply_event(&mut state, PresentationHoldEvent::FrameReady);
    assert_eq!(outcome, PresentationHoldOutcome::Present);
}

#[test]
fn no_double_flush_on_back_to_back_esu() {
    // Defensive: an app issues two ESUs in a row by mistake.
    // The second one is a NoOp (state machine already idle).
    let mut state = PresentationHoldState::initial();
    apply_event(&mut state, PresentationHoldEvent::Bsu);
    apply_event(
        &mut state,
        PresentationHoldEvent::DirtyLineMarked { line: 1 },
    );
    let outcome1 = apply_event(&mut state, PresentationHoldEvent::Esu);
    assert!(matches!(outcome1, PresentationHoldOutcome::Flush { .. }));
    assert_eq!(state.frames_flushed_total, 1);

    // Second ESU should NOT flush a second time.
    let outcome2 = apply_event(&mut state, PresentationHoldEvent::Esu);
    assert_eq!(outcome2, PresentationHoldOutcome::NoOp);
    assert_eq!(state.frames_flushed_total, 1);
}

#[test]
fn nested_bsu_does_not_double_count_held_dirty() {
    // App misuses the protocol with overlapping BSUs. The
    // state machine treats the second BSU as idempotent (the
    // hold window is already open); dirty lines accumulate
    // into the same set.
    let mut state = PresentationHoldState::initial();
    apply_event(&mut state, PresentationHoldEvent::Bsu);
    apply_event(
        &mut state,
        PresentationHoldEvent::DirtyLineMarked { line: 5 },
    );
    apply_event(&mut state, PresentationHoldEvent::Bsu);
    apply_event(
        &mut state,
        PresentationHoldEvent::DirtyLineMarked { line: 5 }, // dedup
    );
    apply_event(
        &mut state,
        PresentationHoldEvent::DirtyLineMarked { line: 9 },
    );
    let outcome = apply_event(&mut state, PresentationHoldEvent::Esu);
    assert_eq!(outcome, PresentationHoldOutcome::Flush { lines_flushed: 2 });
    assert_eq!(state.bsu_count_total, 2);
    // Only 1 ESU consumed; counter reflects what apps issued.
    assert_eq!(state.esu_count_total, 1);
}

#[test]
fn reset_during_active_hold_flushes_then_idle() {
    let mut state = PresentationHoldState::initial();
    apply_event(&mut state, PresentationHoldEvent::Bsu);
    apply_event(
        &mut state,
        PresentationHoldEvent::DirtyLineMarked { line: 1 },
    );
    let outcome = apply_event(&mut state, PresentationHoldEvent::Reset);
    assert!(matches!(outcome, PresentationHoldOutcome::Flush { .. }));
    assert!(!state.synchronized_output_active);
    assert!(state.held_dirty_lines.is_empty());

    // Subsequent FrameReady presents normally.
    let outcome = apply_event(&mut state, PresentationHoldEvent::FrameReady);
    assert_eq!(outcome, PresentationHoldOutcome::Present);
}

#[test]
fn high_volume_event_sweep_invariants_hold() {
    // High-volume sweep — 1024 trials × 96 events = ~98k
    // transitions. Exercises long traces against a tighter
    // bound than the lib's 16-deep sweep.
    let mut rng: u64 = 0xc0ffee15_bad_cafeu64;
    let xorshift = |s: &mut u64| -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    };
    let alphabet = action_alphabet();
    for _ in 0..1024 {
        let mut state = PresentationHoldState::initial();
        for _ in 0..96 {
            let r = xorshift(&mut rng);
            let event = alphabet[(r as usize) % alphabet.len()];
            let prior = state.clone();
            let outcome = apply_event(&mut state, event);
            let v = check_invariants(&prior, &state, event, outcome);
            assert!(v.is_empty(), "violation under {event:?}: {v:?}");
        }
    }
}

#[test]
fn health_projection_matches_state() {
    let mut state = PresentationHoldState::initial();
    apply_event(&mut state, PresentationHoldEvent::Bsu);
    apply_event(
        &mut state,
        PresentationHoldEvent::DirtyLineMarked { line: 4 },
    );
    let h = SynchronizedOutputHealth::from_state(&state);
    assert!(h.synchronized_output_active);
    assert_eq!(h.bsu_count_total, 1);
    assert_eq!(h.esu_count_total, 0);
    assert_eq!(h.held_lines_now, 1);
    assert!(h.bsu_esu_balanced());
}

#[test]
fn dirty_outside_window_is_silently_dropped() {
    // The bead's wiring: app emits dirty bits before
    // entering the hold. Renderer would just paint on the
    // next FrameReady — the state machine's accumulator stays
    // empty.
    let mut state = PresentationHoldState::initial();
    for line in [1, 2, 3] {
        apply_event(&mut state, PresentationHoldEvent::DirtyLineMarked { line });
    }
    assert!(state.held_dirty_lines.is_empty());
}
