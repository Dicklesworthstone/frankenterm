//! Property tests for `storage_cardinality_sketch::StorageDistinctSketch` —
//! the alien-uplift §9.4 HyperLogLog wrapper for distinct-count
//! estimates over storage-domain identifiers (pane_ids, session_ids,
//! embedder_ids).
//!
//! The sketch ships at `crates/frankenterm-core/src/storage_cardinality_sketch.rs`
//! with 5 unit tests pinning the obvious cases (empty → zero,
//! repeat-coalesce, counter-independence on a hand-crafted input,
//! ±5% bounded error at 10k and 100k inserts, snapshot field
//! coverage). What the unit tests don't cover are the deterministic
//! invariants under randomized inputs:
//!
//! 1. **Monotonicity**: cardinality estimates never decrease as
//!    more distinct values are inserted. HLL is a sketch over a
//!    register array that takes the max of `rho` for each bucket;
//!    `cardinality()` derives from those maxes monotonically.
//! 2. **Insert-idempotence**: re-inserting the same value never
//!    changes the estimate. The HLL bucket update is `max(rho)`,
//!    so repeated inserts of the same value hit the same register
//!    with the same `rho` and the max is unchanged.
//! 3. **Counter-independence**: recording a pane_id never changes
//!    the session / embedder estimates (and symmetrically). The
//!    three counters are separate `HyperLogLog` instances.
//! 4. **Snapshot consistency**: the snapshot's three estimates
//!    equal the live getters at the snapshot moment.
//! 5. **Standard error stability**: `standard_error()` is a
//!    function of precision only, so it returns the same value
//!    regardless of insertions.
//! 6. **Memory bound stability**: `memory_bytes()` doesn't change
//!    after construction (HLL register arrays are fixed-size).
//!
//! Logs are emitted as structured tracing-json events on each
//! property case so a failing case lands a parseable record of
//! the input + observed estimate — same shape as the prior phase
//! sweeps.

use std::sync::Once;

use frankenterm_core::storage_cardinality_sketch::StorageDistinctSketch;
use proptest::prelude::*;
use tracing::info;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

/// Vectors of u64 pane_ids up to 64 elements. Includes natural
/// duplicates from proptest's small-range u64 distribution.
fn pane_id_vec() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(any::<u64>(), 0..64)
}

/// Session-id strings: short ASCII identifiers with collision
/// pressure (small alphabet keeps duplicates likely).
fn session_id_vec() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z]{1,8}", 0..32)
}

fn embedder_id_vec() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z0-9]{1,12}", 0..16)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Proof obligation 1 (monotonicity, panes):** appending
    /// MORE pane_id observations never DECREASES the cardinality
    /// estimate. Fundamental property of register-max-based HLL.
    #[test]
    fn proptest_storage_cardinality_panes_monotone(
        before in pane_id_vec(),
        after in pane_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        for id in &before {
            s.record_pane_id(*id);
        }
        let est_before = s.estimated_distinct_panes();
        for id in &after {
            s.record_pane_id(*id);
        }
        let est_after = s.estimated_distinct_panes();

        info!(
            test = "panes_monotone",
            before_count = before.len(),
            after_count = after.len(),
            est_before,
            est_after,
            "monotonicity case"
        );

        prop_assert!(
            est_after >= est_before,
            "estimated_distinct_panes must not decrease ({est_before} → {est_after})"
        );
    }

    /// **Proof obligation 1 (monotonicity, sessions):** same
    /// invariant applied to the session counter.
    #[test]
    fn proptest_storage_cardinality_sessions_monotone(
        before in session_id_vec(),
        after in session_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        for id in &before {
            s.record_session_id(id);
        }
        let est_before = s.estimated_distinct_sessions();
        for id in &after {
            s.record_session_id(id);
        }
        prop_assert!(
            s.estimated_distinct_sessions() >= est_before,
            "estimated_distinct_sessions must not decrease"
        );
    }

    /// **Proof obligation 2 (insert idempotence):** re-inserting
    /// the same set of values N times yields the same estimate
    /// as inserting them once. HLL bucket update is max(rho); the
    /// max over the same register is unchanged on re-insertion.
    #[test]
    fn proptest_storage_cardinality_repeat_insert_idempotent(
        ids in pane_id_vec(),
        repeats in 1u32..=8u32,
    ) {
        init_test_tracing_json();

        let mut once = StorageDistinctSketch::new();
        for id in &ids {
            once.record_pane_id(*id);
        }
        let est_once = once.estimated_distinct_panes();

        let mut repeated = StorageDistinctSketch::new();
        for _ in 0..repeats {
            for id in &ids {
                repeated.record_pane_id(*id);
            }
        }
        let est_repeated = repeated.estimated_distinct_panes();

        info!(
            test = "repeat_insert_idempotent",
            unique_input = ids.len(),
            repeats,
            est_once,
            est_repeated,
            "idempotence case"
        );

        prop_assert_eq!(
            est_once, est_repeated,
            "repeated inserts of the same set must not change the estimate"
        );
    }

    /// **Proof obligation 3 (counter independence):** recording
    /// pane_ids never changes the session OR embedder estimates.
    /// The three counters are separate HLL instances.
    #[test]
    fn proptest_storage_cardinality_panes_dont_affect_sessions_or_embedders(
        pane_ids in pane_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        let initial_sessions = s.estimated_distinct_sessions();
        let initial_embedders = s.estimated_distinct_embedders();

        for id in &pane_ids {
            s.record_pane_id(*id);
        }

        prop_assert_eq!(s.estimated_distinct_sessions(), initial_sessions,
            "recording pane_ids must not change session estimate");
        prop_assert_eq!(s.estimated_distinct_embedders(), initial_embedders,
            "recording pane_ids must not change embedder estimate");
    }

    /// **Proof obligation 3 (counter independence, mirror):**
    /// recording sessions doesn't change panes or embedders.
    #[test]
    fn proptest_storage_cardinality_sessions_dont_affect_panes_or_embedders(
        sessions in session_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        for id in &sessions {
            s.record_session_id(id);
        }
        prop_assert_eq!(s.estimated_distinct_panes(), 0u64,
            "recording sessions must not bump pane estimate");
        prop_assert_eq!(s.estimated_distinct_embedders(), 0u64,
            "recording sessions must not bump embedder estimate");
    }

    /// **Proof obligation 3 (counter independence, mirror):**
    /// recording embedders doesn't change panes or sessions.
    #[test]
    fn proptest_storage_cardinality_embedders_dont_affect_panes_or_sessions(
        embedders in embedder_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        for id in &embedders {
            s.record_embedder_id(id);
        }
        prop_assert_eq!(s.estimated_distinct_panes(), 0u64,
            "recording embedders must not bump pane estimate");
        prop_assert_eq!(s.estimated_distinct_sessions(), 0u64,
            "recording embedders must not bump session estimate");
    }

    /// **Proof obligation 4 (snapshot consistency):** the
    /// snapshot's three estimates equal the live getters at the
    /// snapshot moment.
    #[test]
    fn proptest_storage_cardinality_snapshot_matches_live_getters(
        panes in pane_id_vec(),
        sessions in session_id_vec(),
        embedders in embedder_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        for id in &panes { s.record_pane_id(*id); }
        for id in &sessions { s.record_session_id(id); }
        for id in &embedders { s.record_embedder_id(id); }

        let live_panes = s.estimated_distinct_panes();
        let live_sessions = s.estimated_distinct_sessions();
        let live_embedders = s.estimated_distinct_embedders();
        let snap = s.snapshot();

        prop_assert_eq!(snap.estimated_distinct_panes, live_panes);
        prop_assert_eq!(snap.estimated_distinct_sessions, live_sessions);
        prop_assert_eq!(snap.estimated_distinct_embedders, live_embedders);
        prop_assert_eq!(snap.memory_bytes as usize, s.memory_bytes());
    }

    /// **Proof obligation 5 (standard error stability):**
    /// `standard_error()` depends only on precision (constant
    /// across insertions). Insert any sequence; the standard
    /// error before and after must be bit-equal.
    #[test]
    fn proptest_storage_cardinality_standard_error_stable(panes in pane_id_vec()) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        let se_before = s.standard_error();
        for id in &panes {
            s.record_pane_id(*id);
        }
        let se_after = s.standard_error();
        prop_assert_eq!(
            se_before.to_bits(),
            se_after.to_bits(),
            "standard_error must be stable across insertions"
        );
        // Also stable across constructions of fresh sketches.
        let s2 = StorageDistinctSketch::new();
        prop_assert_eq!(
            s.standard_error().to_bits(),
            s2.standard_error().to_bits(),
            "standard_error must match across fresh sketches at the same precision"
        );
    }

    /// **Proof obligation 6 (memory bound stability):**
    /// `memory_bytes()` doesn't change after construction. HLL
    /// uses a fixed-size register array (2^p bytes per counter),
    /// so the per-insertion memory cost is zero.
    #[test]
    fn proptest_storage_cardinality_memory_bytes_stable(
        panes in pane_id_vec(),
        sessions in session_id_vec(),
        embedders in embedder_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        let bytes_before = s.memory_bytes();
        for id in &panes { s.record_pane_id(*id); }
        for id in &sessions { s.record_session_id(id); }
        for id in &embedders { s.record_embedder_id(id); }
        let bytes_after = s.memory_bytes();
        prop_assert_eq!(
            bytes_after, bytes_before,
            "memory_bytes must not grow with insertions"
        );
    }

    /// **Cardinality estimate is loosely bounded**: for any
    /// number of distinct insertions K, the estimate is at most
    /// `2 * K + small fixed slack` (extremely loose upper bound;
    /// the real HLL bound is `K × (1 + 1.04/√m)`, but for small
    /// K the estimator can bias upward, hence the 2× slack to
    /// avoid flakiness).
    #[test]
    fn proptest_storage_cardinality_estimate_within_loose_bound(
        ids in pane_id_vec(),
    ) {
        init_test_tracing_json();
        let mut s = StorageDistinctSketch::new();
        for id in &ids {
            s.record_pane_id(*id);
        }
        let est = s.estimated_distinct_panes();
        // Compute the true distinct count.
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        let true_distinct = sorted.len() as u64;
        // For tiny cardinalities (0..=64), HLL can bias significantly
        // upward (linear-counting region → harmonic-mean transition).
        // The bound `2*K + 8` covers that. For K=0, bound is 8 —
        // still satisfies the typical empty-sketch returns 0 case.
        let upper_bound = 2 * true_distinct + 8;
        prop_assert!(
            est <= upper_bound,
            "estimate {est} > upper bound {upper_bound} for {true_distinct} distinct inputs"
        );
    }
}
