//! Property-based tests for `xor_filter` module.
//!
//! Verifies correctness invariants for both XorFilter (8-bit) and
//! XorFilter16 (16-bit):
//! - No false negatives (all inserted keys found)
//! - False positive rate within bounds
//! - Duplicate handling
//! - Storage efficiency bounds
//! - Serde roundtrip preservation
//! - Construction determinism
//! - Clone preserves membership

use frankenterm_core::xor_filter::{XorFilter, XorFilter16, XorFilterStats};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn key_set_strategy(max_len: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(any::<u64>(), 1..max_len)
}

fn small_key_set_strategy(max_len: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..10000, 1..max_len)
}

// ── XorFilter (8-bit) tests ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // ── No false negatives (8-bit) ───────────────────────────────

    #[test]
    fn no_false_negatives_8bit(keys in key_set_strategy(200)) {
        let filter = XorFilter::build(&keys).unwrap();
        for &k in &keys {
            prop_assert!(filter.contains(k), "false negative for key {}", k);
        }
    }

    // ── Num keys counts unique (8-bit) ───────────────────────────

    #[test]
    fn num_keys_is_unique_count_8bit(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(filter.num_keys(), unique.len());
    }

    // ── FP rate bounded (8-bit) ──────────────────────────────────

    #[test]
    fn fp_rate_bounded_8bit(keys in small_key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();

        let max_key = keys.iter().copied().max().unwrap_or(0);
        let test_start = max_key + 1;
        let test_count = 5000u64;

        let fp_count = (test_start..test_start + test_count)
            .filter(|&k| filter.contains(k))
            .count();
        let fp_rate = fp_count as f64 / test_count as f64;

        // 8-bit fingerprint: theoretical ~1/255 ≈ 0.39%. Allow up to 3%.
        prop_assert!(
            fp_rate < 0.03,
            "FP rate too high: {:.4} ({}/{})",
            fp_rate, fp_count, test_count
        );
    }

    // ── Storage efficiency (8-bit) ───────────────────────────────

    #[test]
    fn bits_per_key_bounded_8bit(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let bpk = filter.bits_per_key();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() >= 50 {
            prop_assert!(bpk < 15.0, "bits per key too high: {:.2}", bpk);
        }
        prop_assert!(bpk > 0.0, "bits per key should be positive");
    }

    // ── Size bytes positive (8-bit) ──────────────────────────────

    #[test]
    fn size_bytes_positive_8bit(keys in key_set_strategy(50)) {
        let filter = XorFilter::build(&keys).unwrap();
        prop_assert!(filter.size_bytes() > 0);
    }

    // ── Not empty after build (8-bit) ────────────────────────────

    #[test]
    fn not_empty_after_build_8bit(keys in key_set_strategy(50)) {
        let filter = XorFilter::build(&keys).unwrap();
        prop_assert!(!filter.is_empty());
    }

    // ── Serde roundtrip (8-bit) ──────────────────────────────────

    #[test]
    fn serde_roundtrip_8bit(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let json = serde_json::to_string(&filter).unwrap();
        let restored: XorFilter = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.num_keys(), filter.num_keys());
        prop_assert_eq!(restored.size_bytes(), filter.size_bytes());

        for &k in &keys {
            prop_assert!(restored.contains(k), "false negative after roundtrip for {}", k);
        }
    }

    // ── Duplicates handled (8-bit) ───────────────────────────────

    #[test]
    fn duplicates_handled_8bit(keys in small_key_set_strategy(30)) {
        let mut doubled = keys.clone();
        doubled.extend_from_slice(&keys);
        let filter = XorFilter::build(&doubled).unwrap();

        for &k in &keys {
            prop_assert!(filter.contains(k));
        }

        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(filter.num_keys(), unique.len());
    }

    // ── Byte API consistency (8-bit) ─────────────────────────────

    #[test]
    fn byte_api_no_false_negatives_8bit(
        keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..20), 1..50)
    ) {
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let filter = XorFilter::from_bytes(&key_refs).unwrap();

        for key in &keys {
            prop_assert!(filter.contains_bytes(key), "false negative for byte key");
        }
    }

    // ── Build always succeeds (8-bit) ────────────────────────────

    #[test]
    fn build_always_succeeds_8bit(keys in key_set_strategy(200)) {
        let result = XorFilter::build(&keys);
        prop_assert!(result.is_some(), "build failed for {} keys", keys.len());
    }

    // ── Empty contains returns false (8-bit) ─────────────────────

    #[test]
    fn empty_filter_contains_nothing_8bit(key in any::<u64>()) {
        let filter = XorFilter::build(&[]).unwrap();
        prop_assert!(!filter.contains(key));
    }

    // ── Determinism (8-bit) ──────────────────────────────────────

    #[test]
    fn construction_deterministic_8bit(keys in key_set_strategy(100)) {
        let f1 = XorFilter::build(&keys).unwrap();
        let f2 = XorFilter::build(&keys).unwrap();
        prop_assert_eq!(f1.num_keys(), f2.num_keys());
        prop_assert_eq!(f1.size_bytes(), f2.size_bytes());

        // Both must agree on all input keys
        for &k in &keys {
            prop_assert_eq!(f1.contains(k), f2.contains(k));
        }
    }

    // ── Clone preserves membership (8-bit) ───────────────────────

    #[test]
    fn clone_preserves_membership_8bit(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let cloned = filter.clone();
        prop_assert_eq!(filter.num_keys(), cloned.num_keys());
        for &k in &keys {
            prop_assert_eq!(filter.contains(k), cloned.contains(k));
        }
    }

    // ── Stats consistency (8-bit) ────────────────────────────────

    #[test]
    fn stats_consistent_8bit(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let stats = filter.stats();
        prop_assert_eq!(stats.num_keys, filter.num_keys());
        prop_assert_eq!(stats.memory_bytes, filter.size_bytes());
        prop_assert_eq!(stats.fingerprint_bits, 8);

        // Stats serde roundtrip
        let json = serde_json::to_string(&stats).unwrap();
        let back: XorFilterStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.num_keys, back.num_keys);
        prop_assert_eq!(stats.fingerprint_bits, back.fingerprint_bits);
    }
}

// ── XorFilter16 (16-bit) tests ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    // ── No false negatives (16-bit) ──────────────────────────────

    #[test]
    fn no_false_negatives_16bit(keys in key_set_strategy(200)) {
        let filter = XorFilter16::build(&keys).unwrap();
        for &k in &keys {
            prop_assert!(filter.contains(k), "false negative for key {}", k);
        }
    }

    // ── Num keys counts unique (16-bit) ──────────────────────────

    #[test]
    fn num_keys_is_unique_count_16bit(keys in key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(filter.num_keys(), unique.len());
    }

    // ── FP rate bounded (16-bit) ─────────────────────────────────

    #[test]
    fn fp_rate_bounded_16bit(keys in small_key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();

        let max_key = keys.iter().copied().max().unwrap_or(0);
        let test_start = max_key + 1;
        let test_count = 10_000u64;

        let fp_count = (test_start..test_start + test_count)
            .filter(|&k| filter.contains(k))
            .count();
        let fp_rate = fp_count as f64 / test_count as f64;

        // 16-bit fingerprint: theoretical ~1/65535 ≈ 0.0015%.
        // Allow up to 0.5% for statistical variation with small samples.
        prop_assert!(
            fp_rate < 0.005,
            "16-bit FP rate too high: {:.6} ({}/{})",
            fp_rate, fp_count, test_count
        );
    }

    // ── Storage efficiency (16-bit) ──────────────────────────────

    #[test]
    fn bits_per_key_bounded_16bit(keys in key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();
        let bpk = filter.bits_per_key();
        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        if unique.len() >= 50 {
            // Small/medium sets pay fixed construction overhead (+32 slots),
            // so 16-bit bpk can be notably above the asymptotic ~19.68.
            prop_assert!(bpk < 35.0, "16-bit bpk too high: {:.2}", bpk);
            prop_assert!(bpk > 10.0, "16-bit bpk too low: {:.2}", bpk);
        }
        prop_assert!(bpk > 0.0, "bits per key should be positive");
    }

    // ── Serde roundtrip (16-bit) ─────────────────────────────────

    #[test]
    fn serde_roundtrip_16bit(keys in key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();
        let json = serde_json::to_string(&filter).unwrap();
        let restored: XorFilter16 = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(restored.num_keys(), filter.num_keys());
        prop_assert_eq!(restored.size_bytes(), filter.size_bytes());

        for &k in &keys {
            prop_assert!(restored.contains(k), "false negative after 16-bit roundtrip for {}", k);
        }
    }

    // ── Duplicates handled (16-bit) ──────────────────────────────

    #[test]
    fn duplicates_handled_16bit(keys in small_key_set_strategy(30)) {
        let mut doubled = keys.clone();
        doubled.extend_from_slice(&keys);
        let filter = XorFilter16::build(&doubled).unwrap();

        for &k in &keys {
            prop_assert!(filter.contains(k));
        }

        let mut unique = keys.clone();
        unique.sort_unstable();
        unique.dedup();
        prop_assert_eq!(filter.num_keys(), unique.len());
    }

    // ── Build always succeeds (16-bit) ───────────────────────────

    #[test]
    fn build_always_succeeds_16bit(keys in key_set_strategy(200)) {
        let result = XorFilter16::build(&keys);
        prop_assert!(result.is_some(), "16-bit build failed for {} keys", keys.len());
    }

    // ── Empty filter (16-bit) ────────────────────────────────────

    #[test]
    fn empty_filter_16bit(key in any::<u64>()) {
        let filter = XorFilter16::build(&[]).unwrap();
        prop_assert!(!filter.contains(key));
        prop_assert!(filter.is_empty());
    }

    // ── Determinism (16-bit) ─────────────────────────────────────

    #[test]
    fn construction_deterministic_16bit(keys in key_set_strategy(100)) {
        let f1 = XorFilter16::build(&keys).unwrap();
        let f2 = XorFilter16::build(&keys).unwrap();
        prop_assert_eq!(f1.num_keys(), f2.num_keys());
        prop_assert_eq!(f1.size_bytes(), f2.size_bytes());

        for &k in &keys {
            prop_assert_eq!(f1.contains(k), f2.contains(k));
        }
    }

    // ── Clone preserves membership (16-bit) ──────────────────────

    #[test]
    fn clone_preserves_membership_16bit(keys in key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();
        let cloned = filter.clone();
        prop_assert_eq!(filter.num_keys(), cloned.num_keys());
        for &k in &keys {
            prop_assert_eq!(filter.contains(k), cloned.contains(k));
        }
    }

    // ── Stats consistency (16-bit) ───────────────────────────────

    #[test]
    fn stats_consistent_16bit(keys in key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();
        let stats = filter.stats();
        prop_assert_eq!(stats.num_keys, filter.num_keys());
        prop_assert_eq!(stats.memory_bytes, filter.size_bytes());
        prop_assert_eq!(stats.fingerprint_bits, 16);
    }

    // ── 16-bit more precise than 8-bit ──────────────────────────

    #[test]
    fn fp_rate_16bit_lower_than_8bit(keys in small_key_set_strategy(100)) {
        let f8 = XorFilter::build(&keys).unwrap();
        let f16 = XorFilter16::build(&keys).unwrap();

        let max_key = keys.iter().copied().max().unwrap_or(0);
        let test_start = max_key + 1;
        let test_count = 20_000u64;

        let fp8 = (test_start..test_start + test_count)
            .filter(|&k| f8.contains(k))
            .count();
        let fp16 = (test_start..test_start + test_count)
            .filter(|&k| f16.contains(k))
            .count();

        // 16-bit should have fewer or equal false positives
        // (with small probability of equality at zero)
        prop_assert!(
            fp16 <= fp8 + 5, // Allow small statistical noise
            "16-bit FP ({}) should be <= 8-bit FP ({}) + noise",
            fp16, fp8
        );
    }

    // ── Byte API (16-bit) ────────────────────────────────────────

    #[test]
    fn byte_api_no_false_negatives_16bit(
        keys in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..20), 1..50)
    ) {
        let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let filter = XorFilter16::from_bytes(&key_refs).unwrap();

        for key in &keys {
            prop_assert!(filter.contains_bytes(key), "16-bit false negative for byte key");
        }
    }

    // ── Memory proportional (16-bit ≈ 2× 8-bit) ─────────────────

    #[test]
    fn memory_16bit_double_8bit(keys in key_set_strategy(100)) {
        let f8 = XorFilter::build(&keys).unwrap();
        let f16 = XorFilter16::build(&keys).unwrap();

        let ratio = f16.size_bytes() as f64 / f8.size_bytes() as f64;
        prop_assert!(
            (1.8..2.2).contains(&ratio),
            "memory ratio {:.2} not close to 2.0", ratio
        );
    }

    // ── Theoretical FP rates are distinct ──────────────────────

    #[test]
    fn theoretical_fp_rates_differ(_dummy in 0..1u8) {
        let fp8 = XorFilter::theoretical_fp_rate();
        let fp16 = XorFilter16::theoretical_fp_rate();
        prop_assert!(fp8 > fp16, "8-bit fp rate {} should exceed 16-bit {}", fp8, fp16);
        prop_assert!(fp8 > 0.0);
        prop_assert!(fp16 > 0.0);
        prop_assert!(fp8 < 1.0);
        prop_assert!(fp16 < 1.0);
    }

    // ── try_build consistency (8-bit) ────────────────────────────

    #[test]
    fn try_build_matches_build_8bit(keys in key_set_strategy(100)) {
        let opt = XorFilter::build(&keys);
        let res = XorFilter::try_build(&keys);
        match (opt, res) {
            (Some(f1), Ok(f2)) => {
                prop_assert_eq!(f1.num_keys(), f2.num_keys());
                for &k in &keys {
                    prop_assert_eq!(f1.contains(k), f2.contains(k));
                }
            }
            (None, Err(_)) => {} // Both failed — ok
            _ => prop_assert!(false, "build and try_build disagree"),
        }
    }
}

// ── XorFilterStats: strategy + serde ────────────────────────────────

fn arb_xor_filter_stats() -> impl Strategy<Value = XorFilterStats> {
    (
        1usize..10_000,                     // num_keys
        1usize..50_000,                     // table_len
        1usize..20_000,                     // block_length
        prop_oneof![Just(8u8), Just(16u8)], // fingerprint_bits
        1usize..500_000,                    // memory_bytes
        1.0f64..50.0,                       // bits_per_key
        0.0f64..1.0,                        // theoretical_fp_rate
        any::<u64>(),                       // seed
    )
        .prop_map(|(nk, tl, bl, fb, mb, bpk, tfp, seed)| XorFilterStats {
            num_keys: nk,
            table_len: tl,
            block_length: bl,
            fingerprint_bits: fb,
            memory_bytes: mb,
            bits_per_key: bpk,
            theoretical_fp_rate: tfp,
            seed,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// XorFilterStats serde roundtrip preserves all fields.
    #[test]
    fn prop_stats_serde_roundtrip(stats in arb_xor_filter_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        let back: XorFilterStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.num_keys, stats.num_keys);
        prop_assert_eq!(back.table_len, stats.table_len);
        prop_assert_eq!(back.block_length, stats.block_length);
        prop_assert_eq!(back.fingerprint_bits, stats.fingerprint_bits);
        prop_assert_eq!(back.memory_bytes, stats.memory_bytes);
        prop_assert!((back.bits_per_key - stats.bits_per_key).abs() < 1e-9);
        prop_assert!((back.theoretical_fp_rate - stats.theoretical_fp_rate).abs() < 1e-9);
        prop_assert_eq!(back.seed, stats.seed);
    }

    /// XorFilterStats JSON keys are present.
    #[test]
    fn prop_stats_json_keys(stats in arb_xor_filter_stats()) {
        let json = serde_json::to_string(&stats).unwrap();
        prop_assert!(json.contains("\"num_keys\""));
        prop_assert!(json.contains("\"table_len\""));
        prop_assert!(json.contains("\"block_length\""));
        prop_assert!(json.contains("\"fingerprint_bits\""));
        prop_assert!(json.contains("\"memory_bytes\""));
        prop_assert!(json.contains("\"bits_per_key\""));
        prop_assert!(json.contains("\"theoretical_fp_rate\""));
        prop_assert!(json.contains("\"seed\""));
    }

    /// XorFilterStats from built filter has 8-bit fingerprint_bits.
    #[test]
    fn prop_stats_from_8bit_filter(keys in key_set_strategy(100)) {
        let filter = XorFilter::build(&keys).unwrap();
        let stats = filter.stats();
        prop_assert_eq!(stats.fingerprint_bits, 8);
        prop_assert!(stats.bits_per_key > 0.0);
        prop_assert!(stats.theoretical_fp_rate > 0.0);
        prop_assert!(stats.theoretical_fp_rate < 1.0);
    }

    /// XorFilterStats from built filter has 16-bit fingerprint_bits.
    #[test]
    fn prop_stats_from_16bit_filter(keys in key_set_strategy(100)) {
        let filter = XorFilter16::build(&keys).unwrap();
        let stats = filter.stats();
        prop_assert_eq!(stats.fingerprint_bits, 16);
        prop_assert!(stats.bits_per_key > 0.0);
    }

    /// XorFilterStats Clone preserves all fields.
    #[test]
    fn prop_stats_clone(stats in arb_xor_filter_stats()) {
        let cloned = stats.clone();
        prop_assert_eq!(cloned.num_keys, stats.num_keys);
        prop_assert_eq!(cloned.seed, stats.seed);
        prop_assert_eq!(cloned.fingerprint_bits, stats.fingerprint_bits);
    }

    /// XorFilterStats Debug is non-empty.
    #[test]
    fn prop_stats_debug_nonempty(stats in arb_xor_filter_stats()) {
        let debug = format!("{:?}", stats);
        prop_assert!(!debug.is_empty());
    }
}
