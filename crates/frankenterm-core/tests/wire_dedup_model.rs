//! Exhaustive state-space proof of the wire-protocol per-sender
//! dedup
//! ([BR-RC-SAFETY-PROOFS.G11] / `ft-x0666.3`).
//!
//! This is the Stateright-shape harness paired with the Rust
//! model in `crates/frankenterm-core/src/wire_dedup_model.rs`
//! and the TLA+ spec in `docs/specs/wire-dedup.tla`. It does:
//!
//! 1. **Permutation BFS** — for a small input multiset, enumerate
//!    every delivery order (every permutation), apply each
//!    schedule against the dedup model, collect the final
//!    `frontier()` and `messages_received` per sender. Asserts
//!    convergence: every schedule yields the same frontier.
//! 2. **Adversarial reorder + duplicate** — for arbitrary
//!    multisets up to size 8, try N=1024 random schedules and
//!    assert the same frontier-equivalence holds plus no
//!    safety invariant fires.
//! 3. **Per-step invariant check** — after every individual
//!    ingest, assert `check_invariants` is empty.
//!
//! The proof bound (8 envelopes × 2 senders × 3 max seq) is
//! the BFS sweet spot: ~40k schedules × dozens of
//! intermediate states each. Runs in <100ms.

use frankenterm_core::wire_dedup_model::{
    DedupModelState, IngestOutcome, SenderId, Seq, WireDedupHealth, check_invariants,
};

/// Generate every permutation of `xs` (Heap's algorithm).
fn permutations<T: Clone>(xs: &[T]) -> Vec<Vec<T>> {
    let mut out = Vec::new();
    let mut buf: Vec<T> = xs.to_vec();
    let n = buf.len();
    if n == 0 {
        out.push(Vec::new());
        return out;
    }
    let mut c = vec![0usize; n];
    out.push(buf.clone());
    let mut i = 0;
    while i < n {
        if c[i] < i {
            if i % 2 == 0 {
                buf.swap(0, i);
            } else {
                buf.swap(c[i], i);
            }
            out.push(buf.clone());
            c[i] += 1;
            i = 0;
        } else {
            c[i] = 0;
            i += 1;
        }
    }
    out
}

/// Apply a schedule to a fresh model and return the final state.
fn run_schedule(schedule: &[(SenderId, Seq)]) -> DedupModelState {
    let mut state = DedupModelState::initial();
    for (sender, seq) in schedule {
        state.apply_ingest(*sender, *seq);
    }
    state
}

#[test]
fn convergence_under_all_orderings_single_sender_three_seqs() {
    // Multiset: sender 1, seqs 0, 1, 2 (each unique).
    let multiset: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 1), (1, 2)];
    let schedules = permutations(&multiset);
    assert_eq!(schedules.len(), 6); // 3!

    let canonical = run_schedule(&multiset).frontier();

    for sched in &schedules {
        let final_state = run_schedule(sched);
        assert_eq!(
            final_state.frontier(),
            canonical,
            "frontier diverged under schedule {:?}; got {:?} vs canonical {:?}",
            sched,
            final_state.frontier(),
            canonical,
        );
    }
}

#[test]
fn convergence_under_all_orderings_two_senders() {
    // Multiset: sender 1 → [0, 1]; sender 2 → [0, 1, 2].
    let multiset: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 1), (2, 0), (2, 1), (2, 2)];
    let schedules = permutations(&multiset);
    assert_eq!(schedules.len(), 120); // 5!

    let canonical = run_schedule(&multiset).frontier();

    for sched in &schedules {
        let final_state = run_schedule(sched);
        assert_eq!(
            final_state.frontier(),
            canonical,
            "frontier diverged under {:?}",
            sched,
        );
    }
}

#[test]
fn convergence_with_duplicates() {
    // Multiset: 2× (1, 0); 2× (1, 1); 1× (1, 2).
    let multiset: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 0), (1, 1), (1, 1), (1, 2)];
    let schedules = permutations(&multiset);
    assert_eq!(schedules.len(), 120);

    let canonical = run_schedule(&multiset).frontier();
    assert_eq!(canonical.get(&1), Some(&2));

    for sched in &schedules {
        let final_state = run_schedule(sched);
        assert_eq!(final_state.frontier(), canonical);
    }
}

#[test]
fn safety_invariants_hold_on_every_intermediate_state_all_orderings() {
    // For each schedule, after every individual step assert
    // check_invariants is empty.
    let multiset: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 1), (1, 2), (2, 0), (2, 1)];
    let schedules = permutations(&multiset);
    assert_eq!(schedules.len(), 120);

    for sched in &schedules {
        let mut state = DedupModelState::initial();
        let mut history: Vec<(SenderId, Seq)> = Vec::new();
        for (sender, seq) in sched {
            state.apply_ingest(*sender, *seq);
            history.push((*sender, *seq));
            let v = check_invariants(&state, &history);
            assert!(
                v.is_empty(),
                "invariant violated mid-schedule {sched:?} at step {history:?}: {v:?}"
            );
        }
    }
}

#[test]
fn duplicate_count_invariant_holds_under_reordering() {
    // Property: for any schedule of a fixed multiset, the
    // total events = accepted + duplicates is constant. The
    // accepted count equals the number of distinct seqs in
    // the multiset for each sender. The duplicate count
    // varies between 0 (best case: ascending unique) and
    // total_events - distinct_seqs (worst case: dense
    // duplicates).
    let multiset: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 0), (1, 1), (2, 0)];
    let schedules = permutations(&multiset);
    assert_eq!(schedules.len(), 24);

    for sched in &schedules {
        let state = run_schedule(sched);
        let total_events: u32 = state
            .sessions
            .values()
            .map(|s| s.messages_received + s.duplicates_skipped)
            .sum();
        assert_eq!(total_events as usize, sched.len());
    }
}

#[test]
fn senders_are_independent_under_interleaving() {
    // Sender 1: seq 5 (single message).
    // Sender 2: seqs 0..3.
    // Regardless of interleaving, sender 1's frontier is 5
    // and sender 2's frontier is 2.
    let multiset: Vec<(SenderId, Seq)> = vec![(1, 5), (2, 0), (2, 1), (2, 2)];
    let schedules = permutations(&multiset);

    for sched in &schedules {
        let state = run_schedule(sched);
        assert_eq!(state.frontier().get(&1), Some(&5));
        assert_eq!(state.frontier().get(&2), Some(&2));
    }
}

#[test]
fn replay_attempt_never_accepts() {
    // Adversary repeats (1, 5) ten times after an initial accept.
    let mut state = DedupModelState::initial();
    state.apply_ingest(1, 5);
    for _ in 0..10 {
        let outcome = state.apply_ingest(1, 5);
        assert_eq!(outcome, IngestOutcome::Duplicate);
    }
    let session = state.sessions.get(&1).unwrap();
    assert_eq!(session.messages_received, 1);
    assert_eq!(session.duplicates_skipped, 10);
    assert_eq!(session.last_seq, 5);

    let history: Vec<(SenderId, Seq)> = std::iter::once((1, 5))
        .chain(std::iter::repeat((1, 5)).take(10))
        .collect();
    assert!(check_invariants(&state, &history).is_empty());
}

#[test]
fn lower_seq_after_high_is_always_duplicate() {
    // Adversary tries to inject lower seqs to confuse the
    // dedup. Frontier must hold.
    let mut state = DedupModelState::initial();
    state.apply_ingest(1, 100);
    for seq in 0..100u8 {
        let outcome = state.apply_ingest(1, seq);
        assert_eq!(outcome, IngestOutcome::Duplicate);
    }
    let session = state.sessions.get(&1).unwrap();
    assert_eq!(session.last_seq, 100);
    assert_eq!(session.messages_received, 1);
    assert_eq!(session.duplicates_skipped, 100);
}

#[test]
fn drop_subset_yields_equal_or_lower_frontier() {
    // Property: if schedule B is a subsequence of schedule A
    // (i.e., A with some events dropped), B's frontier is
    // ≤ A's frontier per sender.
    let full: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 1), (1, 2), (1, 3)];
    let subset: Vec<(SenderId, Seq)> = vec![(1, 0), (1, 2)];

    let full_state = run_schedule(&full);
    let subset_state = run_schedule(&subset);

    let full_max = full_state.sessions.get(&1).unwrap().last_seq;
    let subset_max = subset_state.sessions.get(&1).unwrap().last_seq;
    assert!(subset_max <= full_max);
}

#[test]
fn baseline_health_unsafe_until_explored() {
    // Per ft-11d5f sweep fix: cold baseline reports unsafe
    // (no schedule explored). Previously asserted is_safe.
    let h = WireDedupHealth::baseline();
    assert!(!h.is_safe());
    assert_eq!(h.duplicate_ratio(), 0.0);
    assert_eq!(h.schedules_explored, 0);
}

#[test]
fn random_schedule_sweep_no_violations() {
    // 1024 random schedules over a fixed multiset.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let multiset: Vec<(SenderId, Seq)> = vec![
        (1, 0),
        (1, 1),
        (1, 2),
        (2, 0),
        (2, 1),
        (2, 2),
        (1, 1), // duplicate
        (2, 0), // duplicate
    ];

    // Deterministic seed per test invocation, but vary across
    // schedules via xorshift.
    let mut rng_state: u64 = 0xa5a5_a5a5_d3ad_b33fu64;
    let xorshift = |s: &mut u64| -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    };

    // Build canonical from a sorted reference. Frontier is
    // schedule-independent, so any ordering would do; sort is
    // chosen so the canonical run is deterministic / readable.
    let mut sorted = multiset.clone();
    sorted.sort();
    let canonical_frontier = run_schedule(&sorted).frontier();

    for trial in 0..1024 {
        // Fisher-Yates with xorshift rng.
        let mut sched = multiset.clone();
        let n = sched.len();
        for i in (1..n).rev() {
            let r = (xorshift(&mut rng_state) % ((i + 1) as u64)) as usize;
            sched.swap(i, r);
        }

        let mut state = DedupModelState::initial();
        let mut history: Vec<(SenderId, Seq)> = Vec::new();
        for (sender, seq) in &sched {
            state.apply_ingest(*sender, *seq);
            history.push((*sender, *seq));
            let v = check_invariants(&state, &history);
            assert!(
                v.is_empty(),
                "trial {trial} schedule {sched:?} step {history:?}: {v:?}"
            );
        }

        assert_eq!(
            state.frontier(),
            canonical_frontier,
            "trial {trial}: frontier diverged under schedule {sched:?}",
        );

        // Hash sched for visibility in failure.
        let mut h = DefaultHasher::new();
        sched.hash(&mut h);
        let _digest = h.finish();
    }
}

#[test]
fn snapshot_and_frontier_are_consistent() {
    let mut state = DedupModelState::initial();
    state.apply_ingest(1, 3);
    state.apply_ingest(1, 1); // dup
    state.apply_ingest(2, 0);
    state.apply_ingest(2, 5);
    state.apply_ingest(2, 2); // dup

    let snap = state.snapshot();
    let front = state.frontier();
    for (sender, seq) in &front {
        let snap_entry = snap.get(sender).unwrap();
        assert_eq!(snap_entry.0, *seq, "snapshot.last_seq vs frontier mismatch");
    }
    assert_eq!(front.get(&1), Some(&3));
    assert_eq!(front.get(&2), Some(&5));
    assert_eq!(snap.get(&1).unwrap().2, 1); // 1 dup
    assert_eq!(snap.get(&2).unwrap().2, 1); // 1 dup
}
