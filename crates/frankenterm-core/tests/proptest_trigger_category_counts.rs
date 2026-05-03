//! Property tests for `pattern_trigger::TriggerCategoryCounts` —
//! the array-backed counter substrate that scan_pipeline + every
//! pipeline downstream of it accumulates trigger matches into.
//!
//! ## Why this file exists
//!
//! `proptest_scan_pipeline.rs` (21 KB, ~30 properties) covers
//! the chunked-vs-batch parity surface and treats
//! `TriggerCategoryCounts` as a black box (it asserts
//! `total_trigger_matches() == 0` only on the empty-state edge).
//! This file pins the counter-data-structure invariants directly
//! across randomized add/clear/get sequences:
//!
//! 1. **`count(c)` is the slot value** — `add(c, k)` followed by
//!    `count(c)` returns `k` (or saturating sum across multiple
//!    adds).
//! 2. **`add` saturates at `u64::MAX`** — overflow is bounded,
//!    never wraps. Catches future refactors that swap the
//!    counter representation away from `u64`'s saturating_add.
//! 3. **`get` ↔ `count` correspondence** — `get(&c)` returns
//!    `Some(&n)` when `n > 0`, `None` when `n == 0`; the
//!    underlying value matches `count(c)`.
//! 4. **`iter` yields `TriggerCategory::all()` order** — the
//!    iteration order is stable across builds and matches the
//!    enum's `all()` ordering.
//! 5. **`iter_nonzero` filters zero counts** — every yielded
//!    entry has count > 0; every entry with count > 0 appears.
//! 6. **`clear` returns to zero** — every category's count is 0
//!    after clear; total sum is 0; iter_nonzero is empty.
//! 7. **`as_index` ↔ `from_index` round-trip** — for every
//!    variant `c`, `from_index(c.as_index()) == Some(c)`; for
//!    `i >= TRIGGER_CATEGORY_COUNT`, `from_index(i) == None`.
//! 8. **PartialEq is structural** — two counts are equal iff
//!    every per-category value is equal.
//!
//! Logs are emitted as structured tracing-json events on each
//! property case so failing cases land a parseable record of
//! the input + observed counter state.

use std::sync::Once;

use frankenterm_core::pattern_trigger::{
    TRIGGER_CATEGORY_COUNT, TriggerCategory, TriggerCategoryCounts,
};
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

fn arb_category() -> impl Strategy<Value = TriggerCategory> {
    prop_oneof![
        Just(TriggerCategory::Error),
        Just(TriggerCategory::Warning),
        Just(TriggerCategory::Completion),
        Just(TriggerCategory::Progress),
        Just(TriggerCategory::TestResult),
        Just(TriggerCategory::Custom),
    ]
}

fn arb_add_op() -> impl Strategy<Value = (TriggerCategory, u64)> {
    (arb_category(), 0u64..=10_000_u64)
}

fn arb_op_seq() -> impl Strategy<Value = Vec<(TriggerCategory, u64)>> {
    prop::collection::vec(arb_add_op(), 0..32)
}

/// Reference: build the same counts via a 6-element array using
/// saturating_add. This is the contract the data-structure must
/// preserve.
fn reference_counts(ops: &[(TriggerCategory, u64)]) -> [u64; TRIGGER_CATEGORY_COUNT] {
    let mut out = [0u64; TRIGGER_CATEGORY_COUNT];
    for (cat, delta) in ops {
        let i = cat.as_index();
        out[i] = out[i].saturating_add(*delta);
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// **Property 1 + 2 — count() and add() match the reference
    /// saturating-add counter.** The array-backed
    /// TriggerCategoryCounts must agree with a plain `[u64; 6]`
    /// reference counter on every category for every input
    /// sequence.
    #[test]
    fn proptest_trigger_counts_count_matches_reference(ops in arb_op_seq()) {
        init_test_tracing_json();
        let mut tcc = TriggerCategoryCounts::new();
        for (cat, delta) in &ops {
            tcc.add(*cat, *delta);
        }
        let reference = reference_counts(&ops);

        info!(
            test = "count_matches_reference",
            op_count = ops.len(),
            reference = ?reference,
            "count vs reference case"
        );

        for (i, expected) in reference.iter().enumerate() {
            let cat = TriggerCategory::from_index(i)
                .expect("from_index covers TRIGGER_CATEGORY_COUNT range");
            prop_assert_eq!(
                tcc.count(cat),
                *expected,
                "count({:?}) must match reference",
                cat
            );
        }
    }

    /// **Property 2 (explicit) — add() saturates at u64::MAX.**
    /// Adding u64::MAX twice doesn't wrap; the slot stays at
    /// u64::MAX.
    #[test]
    fn proptest_trigger_counts_add_saturates_at_u64_max(cat in arb_category()) {
        init_test_tracing_json();
        let mut tcc = TriggerCategoryCounts::new();
        tcc.add(cat, u64::MAX);
        prop_assert_eq!(tcc.count(cat), u64::MAX);
        tcc.add(cat, u64::MAX);
        prop_assert_eq!(tcc.count(cat), u64::MAX,
            "second add of u64::MAX must saturate, not wrap");
        tcc.add(cat, 1);
        prop_assert_eq!(tcc.count(cat), u64::MAX,
            "add(1) past saturation must stay at u64::MAX");
    }

    /// **Property 3 — get ↔ count correspondence**: get returns
    /// Some(&n) iff n > 0; the underlying value matches count.
    #[test]
    fn proptest_trigger_counts_get_matches_count_with_zero_filter(ops in arb_op_seq()) {
        init_test_tracing_json();
        let mut tcc = TriggerCategoryCounts::new();
        for (cat, delta) in &ops {
            tcc.add(*cat, *delta);
        }
        for cat in [
            TriggerCategory::Error,
            TriggerCategory::Warning,
            TriggerCategory::Completion,
            TriggerCategory::Progress,
            TriggerCategory::TestResult,
            TriggerCategory::Custom,
        ] {
            let n = tcc.count(cat);
            match tcc.get(&cat) {
                Some(&observed) => {
                    prop_assert_eq!(observed, n,
                        "get({:?}) returned {} != count {}",
                        cat, observed, n);
                    prop_assert!(observed > 0,
                        "get must return None for zero counts");
                }
                None => {
                    prop_assert_eq!(n, 0,
                        "get returned None but count is {} for {:?}", n, cat);
                }
            }
        }
    }

    /// **Property 4 — iter yields TriggerCategory::all() order.**
    /// The yielded categories are exactly the enum variants in
    /// the enum's `all()` order. Counts match per-slot lookups.
    #[test]
    fn proptest_trigger_counts_iter_yields_all_in_enum_order(ops in arb_op_seq()) {
        init_test_tracing_json();
        let mut tcc = TriggerCategoryCounts::new();
        for (cat, delta) in &ops {
            tcc.add(*cat, *delta);
        }
        let yielded: Vec<TriggerCategory> = tcc.iter().map(|(c, _)| c).collect();
        let expected = TriggerCategory::all().to_vec();
        prop_assert_eq!(yielded.clone(), expected,
            "iter must yield TriggerCategory::all() order");

        // And count values match.
        for (cat, n) in tcc.iter() {
            prop_assert_eq!(n, tcc.count(cat),
                "iter count for {:?} must match count()", cat);
        }
        prop_assert_eq!(yielded.len(), TRIGGER_CATEGORY_COUNT);
    }

    /// **Property 5 — iter_nonzero filters zero counts.**
    /// Every yielded entry has count > 0; every entry in the
    /// full iter() with count > 0 also appears in iter_nonzero.
    #[test]
    fn proptest_trigger_counts_iter_nonzero_filters_zeros(ops in arb_op_seq()) {
        init_test_tracing_json();
        let mut tcc = TriggerCategoryCounts::new();
        for (cat, delta) in &ops {
            tcc.add(*cat, *delta);
        }
        let nonzero: Vec<(TriggerCategory, u64)> = tcc.iter_nonzero().collect();
        for (_, n) in &nonzero {
            prop_assert!(*n > 0, "iter_nonzero yielded zero count");
        }

        // Forward direction: every nonzero entry from the full
        // iter() must appear.
        let all_nonzero: Vec<(TriggerCategory, u64)> =
            tcc.iter().filter(|(_, n)| *n > 0).collect();
        prop_assert_eq!(nonzero, all_nonzero,
            "iter_nonzero must equal iter().filter(>0)");
    }

    /// **Property 6 — clear() returns to all-zero.** Every
    /// category's count is 0 after clear; iter_nonzero is empty.
    #[test]
    fn proptest_trigger_counts_clear_returns_to_zero(ops in arb_op_seq()) {
        init_test_tracing_json();
        let mut tcc = TriggerCategoryCounts::new();
        for (cat, delta) in &ops {
            tcc.add(*cat, *delta);
        }
        tcc.clear();
        let nonzero_count = tcc.iter_nonzero().count();
        prop_assert_eq!(nonzero_count, 0,
            "iter_nonzero must be empty after clear");
        for cat in TriggerCategory::all() {
            prop_assert_eq!(tcc.count(cat), 0,
                "count must be 0 for {:?} after clear", cat);
        }
        prop_assert_eq!(tcc, TriggerCategoryCounts::new(),
            "cleared counts must equal a fresh new() instance");
    }

    /// **Property 7 — as_index / from_index round-trip.**
    /// For every variant c, from_index(c.as_index()) == Some(c).
    /// For i >= TRIGGER_CATEGORY_COUNT, from_index(i) == None.
    #[test]
    fn proptest_trigger_category_index_round_trip(
        cat in arb_category(),
        oob in TRIGGER_CATEGORY_COUNT..(TRIGGER_CATEGORY_COUNT + 16),
    ) {
        init_test_tracing_json();
        let i = cat.as_index();
        prop_assert!(i < TRIGGER_CATEGORY_COUNT);
        prop_assert_eq!(TriggerCategory::from_index(i), Some(cat),
            "from_index({}) must round-trip to {:?}", i, cat);
        prop_assert_eq!(TriggerCategory::from_index(oob), None,
            "from_index({}) must be None for out-of-bound index", oob);
    }

    /// **Property 8 — PartialEq is structural across all
    /// categories.** Two counts are equal iff every per-category
    /// value is equal.
    #[test]
    fn proptest_trigger_counts_partial_eq_is_structural(
        ops_a in arb_op_seq(),
        ops_b in arb_op_seq(),
    ) {
        init_test_tracing_json();
        let mut a = TriggerCategoryCounts::new();
        for (cat, delta) in &ops_a {
            a.add(*cat, *delta);
        }
        let mut b = TriggerCategoryCounts::new();
        for (cat, delta) in &ops_b {
            b.add(*cat, *delta);
        }

        let observed_eq = a == b;
        let expected_eq = TriggerCategory::all()
            .iter()
            .all(|cat| a.count(*cat) == b.count(*cat));

        prop_assert_eq!(observed_eq, expected_eq,
            "PartialEq must be structural across all categories");
    }
}
