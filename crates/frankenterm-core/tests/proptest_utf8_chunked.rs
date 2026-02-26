//! Property-based tests for utf8_chunked module (ft-2oph2).
//!
//! Validates that the chunked UTF-8 validator produces correct results
//! across random inputs, boundary positions, and multi-chunk sequences.

use proptest::prelude::*;

use frankenterm_core::utf8_chunked::Utf8ChunkedValidator;

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
        let rt: frankenterm_core::utf8_chunked::Utf8ValidationStats =
            serde_json::from_str(&json).unwrap();
        prop_assert_eq!(rt.valid_bytes, stats.valid_bytes);
        prop_assert_eq!(rt.invalid_bytes, stats.invalid_bytes);
        prop_assert_eq!(rt.replacements, stats.replacements);
    }
}
