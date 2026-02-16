//! Property-based tests for `xor_filter` module.
//!
//! Verifies correctness invariants:
//! - No false negatives (all inserted keys found)
//! - False positive rate within bounds
//! - Duplicate handling
//! - Storage efficiency bounds
//! - Serde roundtrip preservation

use frankenterm_core::xor_filter::XorFilter;
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn key_set_strategy(max_len: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(any::<u64>(), 1..max_len)
}

fn small_key_set_strategy(max_len: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..10000, 1..max_len)
}

// ── Tests ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── No false negatives ───────────────────────────────────────

    #[test]
    fn no_false_negatives(keys in key_set_strategy(200)) {
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            prop_assert!(filter.contains(k), "false negative for key {}", k);
        }
    }

    // ── Num keys counts unique ───────────────────────────────────

    #[test]
    fn num_keys_is_unique_count(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(filter.num_keys(), unique.len());
    }

    // ── FP rate bounded ──────────────────────────────────────────

    #[test]
    fn fp_rate_bounded(keys in small_key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();

        // Test with keys outside the set
        let max_key = keys.iter().copied().max().unwrap_or(0);
        let test_start = max_key + 1;
        let test_count = 5000u64;

        let fp_count = (test_start..test_start + test_count)
            .filter(|&k| filter.contains(k))
            .count();
        let fp_rate = fp_count as f64 / test_count as f64;

        // 8-bit fingerprint → theoretical ~1/255 ≈ 0.39%
        // Allow up to 3% for statistical variation
        prop_assert!(
            fp_rate < 0.03,
            "FP rate too high: {:.4} ({}/{})",
            fp_rate, fp_count, test_count
        );
    }

    // ── Storage efficiency ───────────────────────────────────────

    #[test]
    fn bits_per_key_bounded(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let bpk = filter.bits_per_key();
        // XOR filter should use ~9.84 bits per key for large sets
        // Small sets have high overhead due to minimum capacity (64)
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() >= 50 {
            prop_assert!(bpk < 15.0, "bits per key too high: {:.2}", bpk);
        }
        prop_assert!(bpk > 0.0, "bits per key should be positive");
    }

    // ── Size bytes positive ──────────────────────────────────────

    #[test]
    fn size_bytes_positive(keys in key_set_strategy(50)) {
        let filter = XorFilter::build(&keys).unwrap();
        prop_assert!(filter.size_bytes() > 0);
    }

    // ── Not empty after build ────────────────────────────────────

    #[test]
    fn not_empty_after_build(keys in key_set_strategy(50)) {
        let filter = XorFilter::build(&keys).unwrap();
        prop_assert!(!filter.is_empty());
    }

    // ── Serde roundtrip ──────────────────────────────────────────

    #[test]
    fn serde_roundtrip(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let json = serde_json::to_string(&filter).unwrap();
        let restored: XorFilter = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.num_keys(), filter.num_keys());
        prop_assert_eq!(restored.size_bytes(), filter.size_bytes());

        // All original keys still found
        for &k in &keys {
            prop_assert!(restored.contains(k), "false negative after roundtrip for {}", k);
        }
    }

    // ── Duplicates don't affect membership ───────────────────────

    #[test]
    fn duplicates_handled(keys in small_key_set_strategy(30)) {
        // Double every key
        let mut doubled = keys.clone();
        doubled.extend_from_slice(&keys);
        let filter = XorFilter::build(&doubled).unwrap();

        for &k in &keys {
            prop_assert!(filter.contains(k));
        }

        // num_keys should be unique count
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(filter.num_keys(), unique.len());
    }

    // ── Byte API consistency ─────────────────────────────────────

    #[test]
    fn byte_api_no_false_negatives(
        keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..20), 1..50)
    ) {
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let filter = XorFilter::from_bytes(&key_refs).unwrap();

        for key in &keys {
            prop_assert!(filter.contains_bytes(key), "false negative for byte key");
        }
    }

    // ── Build always succeeds ────────────────────────────────────

    #[test]
    fn build_succeeds(keys in key_set_strategy(200)) {
        let result = XorFilter::build(&keys);
        prop_assert!(result.is_some(), "build failed for {} keys", keys.len());
    }

    // ── Empty contains returns false ─────────────────────────────

    #[test]
    fn empty_filter_contains_nothing(key in any::<u64>()) {
        let filter = XorFilter::build(&[]).unwrap();
        prop_assert!(!filter.contains(key));
    }
}
