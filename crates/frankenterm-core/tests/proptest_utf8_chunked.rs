//! Property-based tests for utf8_chunked module (ft-2oph2).
//!
//! Validates that the chunked UTF-8 validator produces correct results
//! across random inputs, boundary positions, and multi-chunk sequences.

use proptest::prelude::*;

use frankenterm_core::utf8_chunked::{
    ChunkValidation, Utf8ChunkedValidator, Utf8ValidationStats, is_valid_utf8,
    valid_utf8_prefix_len,
};

// =============================================================================
// Strategies
// =============================================================================

fn random_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..4096)
}

fn valid_utf8_text() -> impl Strategy<Value = String> {
    ".{0,2000}"
}

fn mixed_utf8_and_binary() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(
        prop_oneof![
            // Valid ASCII
            (0x20..0x7Eu8).prop_map(|b| vec![b]),
            // Valid 2-byte: é (C3 A9)
            Just(vec![0xC3, 0xA9]),
            // Valid 3-byte: € (E2 82 AC)
            Just(vec![0xE2, 0x82, 0xAC]),
            // Valid 4-byte: 🦀 (F0 9F A6 80)
            Just(vec![0xF0, 0x9F, 0xA6, 0x80]),
            // Invalid byte
            Just(vec![0xFF]),
            // Newline
            Just(vec![0x0A]),
        ],
        1..200,
    )
    .prop_map(|parts| parts.into_iter().flatten().collect())
}

// =============================================================================
// Single-chunk invariant tests
// =============================================================================

proptest! {
    /// Total bytes (valid + invalid + pending) equals input length.
    #[test]
    fn total_bytes_equals_input(data in random_bytes()) {
        let mut v = Utf8ChunkedValidator::new();
        let r = v.validate_chunk(&data);
        let pending = if v.has_pending() {
            // pending bytes aren't counted in valid or invalid until finish
            let f = v.finish();
            f.invalid_bytes
        } else {
            0
        };
        let total = r.valid_bytes + r.invalid_bytes + pending;
        prop_assert_eq!(total, data.len());
    }

    /// Known-valid UTF-8 produces zero invalid bytes.
    #[test]
    fn valid_utf8_no_invalid(text in valid_utf8_text()) {
        let mut v = Utf8ChunkedValidator::new();
        let data = text.as_bytes();
        let r = v.validate_chunk(data);
        prop_assert_eq!(r.invalid_bytes, 0);
        prop_assert_eq!(r.valid_bytes, data.len());
        prop_assert!(!r.has_trailing_partial);
    }

    /// Validity ratio is in [0, 1] range.
    #[test]
    fn validity_ratio_bounded(data in random_bytes()) {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data);
        v.finish();
        let stats = v.stats();
        prop_assert!(stats.validity_ratio >= 0.0);
        prop_assert!(stats.validity_ratio <= 1.0);
    }
}

// =============================================================================
// Multi-chunk consistency tests
// =============================================================================

proptest! {
    /// Splitting valid UTF-8 text at any byte boundary and processing in 2
    /// chunks yields the same total valid bytes as single-chunk processing.
    #[test]
    fn split_valid_utf8_consistent(
        text in valid_utf8_text(),
        split_frac in 0.0..1.0f64,
    ) {
        let data = text.as_bytes();
        if data.is_empty() {
            return Ok(());
        }

        let split_pos = (data.len() as f64 * split_frac) as usize;
        let split_pos = split_pos.min(data.len());

        // Single chunk
        let mut v1 = Utf8ChunkedValidator::new();
        v1.validate_chunk(data);
        v1.finish();
        let stats1 = v1.stats();

        // Two chunks
        let mut v2 = Utf8ChunkedValidator::new();
        v2.validate_chunk(&data[..split_pos]);
        v2.validate_chunk(&data[split_pos..]);
        v2.finish();
        let stats2 = v2.stats();

        // Total valid bytes should match
        prop_assert_eq!(stats1.valid_bytes, stats2.valid_bytes);
        // No invalid bytes in valid UTF-8
        prop_assert_eq!(stats2.invalid_bytes, 0);
    }

    /// Splitting at every byte boundary produces consistent totals.
    #[test]
    fn many_tiny_chunks_consistent(text in "[a-z]{1,50}") {
        let data = text.as_bytes();

        // Single chunk
        let mut v1 = Utf8ChunkedValidator::new();
        v1.validate_chunk(data);
        v1.finish();
        let stats1 = v1.stats();

        // One byte per chunk
        let mut v2 = Utf8ChunkedValidator::new();
        for &b in data {
            v2.validate_chunk(&[b]);
        }
        v2.finish();
        let stats2 = v2.stats();

        prop_assert_eq!(stats1.valid_bytes, stats2.valid_bytes);
        prop_assert_eq!(stats1.invalid_bytes, stats2.invalid_bytes);
    }

    /// Splitting mixed UTF-8/binary at random positions: total bytes
    /// accounted for equals input length.
    #[test]
    fn mixed_split_total_accounted(
        data in mixed_utf8_and_binary(),
        split_frac in 0.0..1.0f64,
    ) {
        if data.is_empty() {
            return Ok(());
        }

        let split_pos = ((data.len() as f64 * split_frac) as usize).min(data.len());

        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data[..split_pos]);
        v.validate_chunk(&data[split_pos..]);
        v.finish();
        let stats = v.stats();

        prop_assert_eq!(
            stats.valid_bytes + stats.invalid_bytes,
            data.len() as u64
        );
    }
}

// =============================================================================
// Accumulation tests
// =============================================================================

proptest! {
    /// Stats accumulate correctly across multiple chunks.
    #[test]
    fn stats_accumulate(
        chunks in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..256),
            1..8,
        )
    ) {
        let mut v = Utf8ChunkedValidator::new();
        let total_input: u64 = chunks.iter().map(|c| c.len() as u64).sum();

        for chunk in &chunks {
            v.validate_chunk(chunk);
        }
        v.finish();
        let stats = v.stats();

        prop_assert_eq!(stats.valid_bytes + stats.invalid_bytes, total_input);
    }

    /// Replacements count is <= invalid_bytes (each replacement covers at
    /// least 1 invalid byte).
    #[test]
    fn replacements_bounded(data in random_bytes()) {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data);
        v.finish();
        let stats = v.stats();
        prop_assert!(stats.replacements <= stats.invalid_bytes);
    }
}

// =============================================================================
// Reset tests
// =============================================================================

proptest! {
    /// After reset, validator behaves as fresh.
    #[test]
    fn reset_then_fresh(
        data1 in random_bytes(),
        data2 in random_bytes(),
    ) {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data1);
        v.reset();

        // Fresh validator for comparison
        let mut fresh = Utf8ChunkedValidator::new();
        fresh.validate_chunk(&data2);
        fresh.finish();

        v.validate_chunk(&data2);
        v.finish();

        let s1 = v.stats();
        let s2 = fresh.stats();
        prop_assert_eq!(s1.valid_bytes, s2.valid_bytes);
        prop_assert_eq!(s1.invalid_bytes, s2.invalid_bytes);
    }
}

// =============================================================================
// Serde roundtrip
// =============================================================================

proptest! {
    /// Utf8ValidationStats survives JSON roundtrip.
    #[test]
    fn stats_serde_roundtrip(data in random_bytes()) {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data);
        v.finish();
        let stats = v.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let rt: Utf8ValidationStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.valid_bytes, stats.valid_bytes);
        prop_assert_eq!(rt.invalid_bytes, stats.invalid_bytes);
        prop_assert_eq!(rt.replacements, stats.replacements);
    }
}

// =============================================================================
// Additional coverage tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// UC-12: ChunkValidation serde roundtrip.
    #[test]
    fn uc12_chunk_validation_serde(
        valid_bytes in 0usize..10000,
        invalid_bytes in 0usize..1000,
        valid_prefix_end in 0usize..10000,
        has_trailing in any::<bool>(),
    ) {
        let cv = ChunkValidation {
            valid_bytes,
            invalid_bytes,
            valid_prefix_end,
            has_trailing_partial: has_trailing,
        };
        let json = serde_json::to_string(&cv).unwrap();
        let back: ChunkValidation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cv.valid_bytes, back.valid_bytes);
        prop_assert_eq!(cv.invalid_bytes, back.invalid_bytes);
        prop_assert_eq!(cv.valid_prefix_end, back.valid_prefix_end);
        prop_assert_eq!(cv.has_trailing_partial, back.has_trailing_partial);
    }

    /// UC-13: Utf8ValidationStats serde roundtrip with arbitrary values.
    #[test]
    fn uc13_stats_serde_arbitrary(
        valid in 0u64..1_000_000,
        invalid in 0u64..1_000_000,
        replacements in 0u64..10000,
    ) {
        let total = valid + invalid;
        let ratio = if total > 0 { valid as f64 / total as f64 } else { 1.0 };
        let stats = Utf8ValidationStats {
            valid_bytes: valid,
            invalid_bytes: invalid,
            replacements,
            validity_ratio: ratio,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: Utf8ValidationStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.valid_bytes, back.valid_bytes);
        prop_assert_eq!(stats.invalid_bytes, back.invalid_bytes);
        prop_assert_eq!(stats.replacements, back.replacements);
        prop_assert!((stats.validity_ratio - back.validity_ratio).abs() < 1e-10);
    }

    /// UC-14: valid_utf8_prefix_len agrees with std::str::from_utf8.
    #[test]
    fn uc14_prefix_len_matches_std(data in random_bytes()) {
        let prefix_len = valid_utf8_prefix_len(&data);
        let std_prefix = match std::str::from_utf8(&data) {
            Ok(_) => data.len(),
            Err(e) => e.valid_up_to(),
        };
        prop_assert_eq!(prefix_len, std_prefix);
    }

    /// UC-15: is_valid_utf8 agrees with std::str::from_utf8.
    #[test]
    fn uc15_is_valid_matches_std(data in random_bytes()) {
        let ours = is_valid_utf8(&data);
        let std_result = std::str::from_utf8(&data).is_ok();
        prop_assert_eq!(ours, std_result);
    }

    /// UC-16: Valid UTF-8 text always has is_valid_utf8 == true.
    #[test]
    fn uc16_valid_string_is_valid(text in valid_utf8_text()) {
        prop_assert!(is_valid_utf8(text.as_bytes()));
    }

    /// UC-17: finish() with no pending returns zero invalid.
    #[test]
    fn uc17_finish_no_pending(text in "[a-z]{0,100}") {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(text.as_bytes());
        prop_assert!(!v.has_pending());
        let f = v.finish();
        prop_assert_eq!(f.invalid_bytes, 0);
    }

    /// UC-18: Empty chunk after non-empty preserves state.
    #[test]
    fn uc18_empty_chunk_preserves_state(data in random_bytes()) {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data);
        let stats_before = v.stats();
        v.validate_chunk(b"");
        let stats_after = v.stats();
        prop_assert_eq!(stats_before.valid_bytes, stats_after.valid_bytes);
        prop_assert_eq!(stats_before.invalid_bytes, stats_after.invalid_bytes);
    }

    /// UC-19: has_pending is true exactly when last chunk ended mid-codepoint.
    #[test]
    fn uc19_has_pending_after_split_multibyte(
        prefix in "[a-z]{0,20}",
    ) {
        let mut v = Utf8ChunkedValidator::new();
        // Feed valid ASCII — no pending
        v.validate_chunk(prefix.as_bytes());
        prop_assert!(!v.has_pending());

        // Feed first byte of é (0xC3 0xA9) — should be pending
        v.validate_chunk(&[0xC3]);
        prop_assert!(v.has_pending());

        // Complete it
        v.validate_chunk(&[0xA9]);
        prop_assert!(!v.has_pending());
    }

    /// UC-20: Splitting multi-byte chars at every byte position produces
    /// same total valid bytes as single-pass.
    #[test]
    fn uc20_multibyte_split_every_position(
        text in "[a-zéèêëà€🦀]{1,30}",
    ) {
        let data = text.as_bytes();

        // Single pass
        let mut v1 = Utf8ChunkedValidator::new();
        v1.validate_chunk(data);
        v1.finish();
        let stats1 = v1.stats();

        // Split at every position
        for split in 0..=data.len() {
            let mut v2 = Utf8ChunkedValidator::new();
            v2.validate_chunk(&data[..split]);
            v2.validate_chunk(&data[split..]);
            v2.finish();
            let stats2 = v2.stats();
            prop_assert_eq!(
                stats1.valid_bytes, stats2.valid_bytes,
                "mismatch at split={}", split
            );
            prop_assert_eq!(stats2.invalid_bytes, 0);
        }
    }

    /// UC-21: Stats validity_ratio is exactly valid/(valid+invalid).
    #[test]
    fn uc21_validity_ratio_exact(data in mixed_utf8_and_binary()) {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&data);
        v.finish();
        let stats = v.stats();
        let total = stats.valid_bytes + stats.invalid_bytes;
        if total > 0 {
            let expected = stats.valid_bytes as f64 / total as f64;
            prop_assert!((stats.validity_ratio - expected).abs() < 1e-10,
                "ratio {} != expected {}", stats.validity_ratio, expected);
        } else {
            prop_assert!((stats.validity_ratio - 1.0).abs() < f64::EPSILON);
        }
    }

    /// UC-22: valid_utf8_prefix_len on valid text returns full length.
    #[test]
    fn uc22_prefix_len_full_on_valid(text in valid_utf8_text()) {
        prop_assert_eq!(valid_utf8_prefix_len(text.as_bytes()), text.len());
    }
}
