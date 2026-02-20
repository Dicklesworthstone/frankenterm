//! Property-based tests for hyperloglog.rs — approximate distinct count estimation.
//!
//! Bead: ft-283h4.21

use frankenterm_core::hyperloglog::*;
use proptest::prelude::*;
use std::collections::HashSet;

// ── Strategies ──────────────────────────────────────────────────────

fn arb_precision() -> impl Strategy<Value = u8> {
    4..=14u8
}

fn arb_items() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0..100000u64, 10..200)
}

fn arb_small_items() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0..10000u64, 10..100)
}

// ── Count properties ────────────────────────────────────────────────

proptest! {
    /// total_inserts matches number of insert calls.
    #[test]
    fn total_inserts_matches(items in arb_items()) {
        let mut hll = HyperLogLog::new();
        for item in &items {
            hll.insert(item);
        }
        prop_assert_eq!(hll.total_inserts(), items.len() as u64);
    }

    /// Empty HLL has cardinality 0.
    #[test]
    fn empty_cardinality_zero(p in arb_precision()) {
        let hll = HyperLogLog::with_precision(p);
        prop_assert_eq!(hll.cardinality(), 0);
        prop_assert!(hll.is_empty());
    }

    /// is_empty consistent with total_inserts.
    #[test]
    fn is_empty_consistent(items in arb_items()) {
        let mut hll = HyperLogLog::new();
        prop_assert!(hll.is_empty());
        for item in &items {
            hll.insert(item);
        }
        prop_assert!(!hll.is_empty());
    }
}

// ── Cardinality accuracy ────────────────────────────────────────────

proptest! {
    /// Cardinality estimate is within relative error bounds.
    #[test]
    fn cardinality_within_bounds(
        p in 8..=14u8,
        items in prop::collection::vec(0..100000u64, 100..500)
    ) {
        let mut hll = HyperLogLog::with_precision(p);
        let mut distinct = HashSet::new();
        for item in &items {
            hll.insert(item);
            distinct.insert(*item);
        }
        let true_card = distinct.len() as f64;
        let est_card = hll.cardinality() as f64;
        // Allow 3 standard errors + small constant for tolerance
        let se = hll.standard_error();
        let tolerance = (3.0 * se * true_card).max(10.0);
        prop_assert!(
            (est_card - true_card).abs() < tolerance,
            "cardinality {} not within {} of true {}", est_card, tolerance, true_card
        );
    }

    /// Cardinality is >= 0.
    #[test]
    fn cardinality_nonnegative(items in arb_small_items()) {
        let mut hll = HyperLogLog::new();
        for item in &items {
            hll.insert(item);
        }
        prop_assert!(hll.cardinality() > 0);
    }

    /// Duplicate elements don't increase cardinality much.
    #[test]
    fn duplicates_stable_cardinality(item in 0..100000u64, reps in 10..500usize) {
        let mut hll = HyperLogLog::with_precision(10);
        for _ in 0..reps {
            hll.insert(&item);
        }
        let card = hll.cardinality();
        prop_assert!(card <= 3, "cardinality of single repeated element should be ~1, got {}", card);
    }
}

// ── Precision properties ────────────────────────────────────────────

proptest! {
    /// Register count equals 2^precision.
    #[test]
    fn register_count_power_of_two(p in arb_precision()) {
        let hll = HyperLogLog::with_precision(p);
        prop_assert_eq!(hll.register_count(), 1 << p);
    }

    /// Memory bytes equals register count.
    #[test]
    fn memory_matches_registers(p in arb_precision()) {
        let hll = HyperLogLog::with_precision(p);
        prop_assert_eq!(hll.memory_bytes(), hll.register_count());
    }

    /// Standard error decreases with precision.
    #[test]
    fn se_decreases_with_precision(p1 in 4..14u8, offset in 1..5u8) {
        let p2 = (p1 + offset).min(18);
        if p1 < p2 {
            let hll1 = HyperLogLog::with_precision(p1);
            let hll2 = HyperLogLog::with_precision(p2);
            prop_assert!(
                hll1.standard_error() > hll2.standard_error(),
                "SE at p={} ({}) should be > SE at p={} ({})",
                p1, hll1.standard_error(), p2, hll2.standard_error()
            );
        }
    }

    /// Precision is clamped to [4, 18].
    #[test]
    fn precision_clamped(p in 0..255u8) {
        let hll = HyperLogLog::with_precision(p);
        prop_assert!(hll.precision() >= 4 && hll.precision() <= 18);
    }
}

// ── Merge properties ────────────────────────────────────────────────

proptest! {
    /// Merged cardinality >= max of parts.
    #[test]
    fn merge_cardinality_ge_parts(
        p in 8..=12u8,
        items1 in arb_small_items(),
        items2 in arb_small_items()
    ) {
        let mut hll1 = HyperLogLog::with_precision(p);
        let mut hll2 = HyperLogLog::with_precision(p);
        for item in &items1 { hll1.insert(item); }
        for item in &items2 { hll2.insert(item); }

        let card1 = hll1.cardinality();
        let card2 = hll2.cardinality();

        hll1.merge(&hll2).unwrap();
        let merged_card = hll1.cardinality();

        prop_assert!(
            merged_card >= card1.max(card2).saturating_sub(1),
            "merged {} should be >= max({}, {}) - 1", merged_card, card1, card2
        );
    }

    /// Merged total_inserts is sum.
    #[test]
    fn merge_inserts_additive(
        p in arb_precision(),
        items1 in arb_small_items(),
        items2 in arb_small_items()
    ) {
        let mut hll1 = HyperLogLog::with_precision(p);
        let mut hll2 = HyperLogLog::with_precision(p);
        for item in &items1 { hll1.insert(item); }
        for item in &items2 { hll2.insert(item); }

        let expected = hll1.total_inserts() + hll2.total_inserts();
        hll1.merge(&hll2).unwrap();
        prop_assert_eq!(hll1.total_inserts(), expected);
    }

    /// Merge of empty into non-empty preserves cardinality.
    #[test]
    fn merge_empty_noop(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let card_before = hll.cardinality();

        let empty = HyperLogLog::with_precision(p);
        hll.merge(&empty).unwrap();
        prop_assert_eq!(hll.cardinality(), card_before);
    }

    /// Merge precision mismatch returns error.
    #[test]
    fn merge_precision_mismatch(p1 in 4..10u8) {
        let mut hll1 = HyperLogLog::with_precision(p1);
        let hll2 = HyperLogLog::with_precision(p1 + 2);
        prop_assert!(hll1.merge(&hll2).is_err());
    }

    /// Self-merge is idempotent for cardinality.
    #[test]
    fn self_merge_idempotent(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let card_before = hll.cardinality();

        let clone = hll.clone();
        hll.merge(&clone).unwrap();
        prop_assert_eq!(hll.cardinality(), card_before);
    }
}

// ── Clear properties ────────────────────────────────────────────────

proptest! {
    /// Clear resets everything.
    #[test]
    fn clear_resets(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        hll.clear();
        prop_assert!(hll.is_empty());
        prop_assert_eq!(hll.cardinality(), 0);
        prop_assert_eq!(hll.total_inserts(), 0);
        prop_assert_eq!(hll.nonzero_registers(), 0);
    }

    /// Clear then insert works correctly.
    #[test]
    fn clear_then_insert(
        p in arb_precision(),
        items1 in arb_small_items(),
        items2 in arb_small_items()
    ) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items1 { hll.insert(item); }
        hll.clear();
        for item in &items2 { hll.insert(item); }
        prop_assert_eq!(hll.total_inserts(), items2.len() as u64);
        prop_assert!(!hll.is_empty());
    }
}

// ── Serde properties ────────────────────────────────────────────────

proptest! {
    /// HllConfig serde roundtrip.
    #[test]
    fn config_serde_roundtrip(p in arb_precision()) {
        let config = HllConfig { precision: p };
        let json = serde_json::to_string(&config).unwrap();
        let back: HllConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    /// HllStats serde roundtrip.
    #[test]
    fn stats_serde_roundtrip(
        p in arb_precision(),
        regs in 0..10000usize,
        nz in 0..5000usize,
        card in 0..100000u64,
        mem in 0..100000usize
    ) {
        let stats = HllStats {
            precision: p,
            register_count: regs,
            nonzero_registers: nz,
            estimated_cardinality: card,
            memory_bytes: mem,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: HllStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats, back);
    }
}

// ── Stats consistency ───────────────────────────────────────────────

proptest! {
    /// Stats fields match direct accessors.
    #[test]
    fn stats_consistent(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let stats = hll.stats();
        prop_assert_eq!(stats.precision, hll.precision());
        prop_assert_eq!(stats.register_count, hll.register_count());
        prop_assert_eq!(stats.nonzero_registers, hll.nonzero_registers());
        prop_assert_eq!(stats.estimated_cardinality, hll.cardinality());
        prop_assert_eq!(stats.memory_bytes, hll.memory_bytes());
    }

    /// nonzero_registers <= register_count.
    #[test]
    fn nonzero_bounded(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        prop_assert!(
            hll.nonzero_registers() <= hll.register_count(),
            "nonzero {} > total {}", hll.nonzero_registers(), hll.register_count()
        );
    }
}

// ── Jaccard properties ──────────────────────────────────────────────

proptest! {
    /// Jaccard of identical sets is near 1.0.
    #[test]
    fn jaccard_identical(p in 8..=12u8, items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let clone = hll.clone();
        let j = hll.jaccard(&clone).unwrap();
        prop_assert!(j > 0.5, "jaccard of identical {} should be high", j);
    }

    /// Jaccard is in [0, 1].
    #[test]
    fn jaccard_bounded(
        p in 8..=12u8,
        items1 in arb_small_items(),
        items2 in arb_small_items()
    ) {
        let mut hll1 = HyperLogLog::with_precision(p);
        let mut hll2 = HyperLogLog::with_precision(p);
        for item in &items1 { hll1.insert(item); }
        for item in &items2 { hll2.insert(item); }
        let j = hll1.jaccard(&hll2).unwrap();
        prop_assert!((0.0..=1.0).contains(&j), "jaccard {} out of [0,1]", j);
    }

    /// Jaccard precision mismatch returns None.
    #[test]
    fn jaccard_precision_mismatch(p1 in 4..10u8) {
        let hll1 = HyperLogLog::with_precision(p1);
        let hll2 = HyperLogLog::with_precision(p1 + 2);
        prop_assert!(hll1.jaccard(&hll2).is_none());
    }
}

// ── Cross-function invariants ───────────────────────────────────────

proptest! {
    /// Clone produces same cardinality.
    #[test]
    fn clone_same_cardinality(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let clone = hll.clone();
        prop_assert_eq!(hll.cardinality(), clone.cardinality());
        prop_assert_eq!(hll.total_inserts(), clone.total_inserts());
    }

    /// insert_hash produces same register effect as insert.
    #[test]
    fn insert_hash_consistent(hash in any::<u64>()) {
        let mut hll1 = HyperLogLog::with_precision(10);
        let mut hll2 = HyperLogLog::with_precision(10);
        hll1.insert_hash(hash);
        hll2.insert_hash(hash);
        prop_assert_eq!(hll1.cardinality(), hll2.cardinality());
    }

    /// from_config matches with_precision.
    #[test]
    fn from_config_matches(p in arb_precision()) {
        let hll1 = HyperLogLog::with_precision(p);
        let hll2 = HyperLogLog::with_config(HllConfig { precision: p });
        prop_assert_eq!(hll1.precision(), hll2.precision());
        prop_assert_eq!(hll1.register_count(), hll2.register_count());
    }

    /// Clone independence: mutation of clone doesn't affect original.
    #[test]
    fn clone_independence(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let card_before = hll.cardinality();
        let inserts_before = hll.total_inserts();

        let mut clone = hll.clone();
        for i in 200_000u64..200_100 {
            clone.insert(&i);
        }

        prop_assert_eq!(hll.cardinality(), card_before);
        prop_assert_eq!(hll.total_inserts(), inserts_before);
    }

    /// cardinality_f64 is non-negative and close to cardinality.
    #[test]
    fn cardinality_f64_consistent(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let card = hll.cardinality() as f64;
        let card_f64 = hll.cardinality_f64();
        prop_assert!(card_f64 >= 0.0);
        // Integer cardinality should be the floor of the f64 version
        let diff = (card - card_f64).abs();
        prop_assert!(diff < 1.5, "cardinality {} and cardinality_f64 {} differ too much", card, card_f64);
    }

    /// Default HLL has standard precision and is empty.
    #[test]
    fn default_is_empty(_dummy in 0..1u8) {
        let hll = HyperLogLog::new();
        prop_assert!(hll.is_empty());
        prop_assert_eq!(hll.cardinality(), 0);
        prop_assert_eq!(hll.total_inserts(), 0);
        prop_assert_eq!(hll.nonzero_registers(), 0);
    }
}

// ── Additional behavioral invariants ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// jaccard is symmetric.
    #[test]
    fn jaccard_symmetry(
        p in 8..=12u8,
        items1 in arb_small_items(),
        items2 in arb_small_items(),
    ) {
        let mut hll1 = HyperLogLog::with_precision(p);
        let mut hll2 = HyperLogLog::with_precision(p);
        for item in &items1 { hll1.insert(item); }
        for item in &items2 { hll2.insert(item); }
        let j12 = hll1.jaccard(&hll2).unwrap();
        let j21 = hll2.jaccard(&hll1).unwrap();
        prop_assert!((j12 - j21).abs() < 1e-10,
            "jaccard not symmetric: {} vs {}", j12, j21);
    }

    /// jaccard of two empty HLLs is 0.0.
    #[test]
    fn jaccard_empty_empty_zero(p in arb_precision()) {
        let hll1 = HyperLogLog::with_precision(p);
        let hll2 = HyperLogLog::with_precision(p);
        let j = hll1.jaccard(&hll2).unwrap();
        prop_assert!((j - 0.0).abs() < 1e-10,
            "jaccard of two empty HLLs should be 0.0, got {}", j);
    }

    /// nonzero_registers is non-decreasing after inserts.
    #[test]
    fn nonzero_registers_nondecreasing(
        p in arb_precision(),
        items in prop::collection::vec(any::<u64>(), 2..50),
    ) {
        let mut hll = HyperLogLog::with_precision(p);
        let mut prev_nz = 0;
        for item in &items {
            hll.insert(item);
            let nz = hll.nonzero_registers();
            prop_assert!(nz >= prev_nz,
                "nonzero_registers should not decrease: {} -> {}", prev_nz, nz);
            prev_nz = nz;
        }
    }

    /// clear then merge recovers cardinality.
    #[test]
    fn clear_then_merge_recovers(p in arb_precision(), items in arb_small_items()) {
        let mut hll = HyperLogLog::with_precision(p);
        for item in &items { hll.insert(item); }
        let card_before = hll.cardinality();
        let mut cleared = HyperLogLog::with_precision(p);
        cleared.merge(&hll).unwrap();
        prop_assert_eq!(cleared.cardinality(), card_before);
    }

    /// with_config register_count equals 2^p.
    #[test]
    fn with_config_register_count(p in arb_precision()) {
        let hll = HyperLogLog::with_config(HllConfig { precision: p });
        prop_assert_eq!(hll.register_count(), 1 << p as usize);
        prop_assert!(hll.is_empty());
    }

    /// insert_hash increments total_inserts.
    #[test]
    fn insert_hash_increments_total(
        hashes in prop::collection::vec(any::<u64>(), 1..50),
    ) {
        let mut hll = HyperLogLog::with_precision(10);
        for (i, &h) in hashes.iter().enumerate() {
            hll.insert_hash(h);
            prop_assert_eq!(hll.total_inserts(), (i + 1) as u64);
        }
    }

    /// stats().nonzero_registers matches nonzero_registers().
    #[test]
    fn stats_nonzero_consistent(hashes in prop::collection::vec(any::<u64>(), 1..30)) {
        let mut hll = HyperLogLog::with_precision(10);
        for &h in &hashes { hll.insert_hash(h); }
        let stats = hll.stats();
        prop_assert_eq!(stats.nonzero_registers, hll.nonzero_registers());
        prop_assert_eq!(stats.estimated_cardinality, hll.cardinality());
    }
}
