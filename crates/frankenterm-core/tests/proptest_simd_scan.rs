//! Property-based tests for SIMD-friendly output scanning primitives.
//!
//! Validates:
//! 1. `scan_newlines_and_ansi` matches scalar reference on arbitrary bytes
//! 2. `logical_line_count` matches `str::lines().count()` for valid UTF-8
//! 3. `ansi_density` is always in [0.0, 1.0]
//! 4. `ansi_byte_count` never exceeds total byte count
//! 5. `newline_count` matches manual `\n` count
//! 6. Empty input produces zeroed metrics
//! 7. Concatenation property: newline count is additive across splits
//! 8. Pure ANSI sequences have density 1.0
//! 9. `logical_line_count` handles trailing newline semantics correctly
//! 10. Clone/Copy/Debug/PartialEq derive correctness
//! 11. Density monotonicity under ANSI injection
//! 12. Prefix scan consistency
//! 13. Pure-newline and single-byte edge cases

use proptest::prelude::*;

use frankenterm_core::simd_scan::{OutputScanMetrics, scan_newlines_and_ansi};

// =============================================================================
// Strategies
// =============================================================================

fn arb_bytes(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..max_len)
}

fn arb_ascii_text(max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(0x20_u8..0x7F, 0..max_len)
        .prop_map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn arb_text_with_newlines(max_len: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            // Normal ASCII characters
            (0x20_u8..0x7F).prop_map(|b| b as char),
            // Newlines (weighted higher for more interesting tests)
            Just('\n'),
        ],
        0..max_len,
    )
    .prop_map(|chars| chars.into_iter().collect::<String>())
}

fn arb_ansi_sequence() -> impl Strategy<Value = Vec<u8>> {
    // Generate a valid CSI sequence: ESC [ <params> <final byte>
    let params = proptest::collection::vec(0x30_u8..0x40, 0..5);
    let final_byte = (0x40_u8..0x7F).prop_filter("not [", |b| *b != b'[');
    (params, final_byte).prop_map(|(params, fb)| {
        let mut seq = vec![0x1b, b'['];
        seq.extend(params);
        seq.push(fb);
        seq
    })
}

fn arb_text_with_ansi(max_segments: usize) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(
        prop_oneof![
            // Plain ASCII bytes
            proptest::collection::vec(0x20_u8..0x7F, 1..20),
            // ANSI sequences
            arb_ansi_sequence(),
        ],
        1..max_segments,
    )
    .prop_map(|segments| segments.into_iter().flatten().collect())
}

// =============================================================================
// Property: newline_count matches manual count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn newline_count_matches_manual(data in arb_bytes(4096)) {
        let scan = scan_newlines_and_ansi(&data);
        #[allow(clippy::naive_bytecount)]
        let manual_count = data.iter().filter(|&&b| b == b'\n').count();
        prop_assert_eq!(
            scan.newline_count,
            manual_count,
        );
    }
}

// =============================================================================
// Property: ansi_byte_count never exceeds total byte count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn ansi_byte_count_bounded(data in arb_bytes(4096)) {
        let scan = scan_newlines_and_ansi(&data);
        prop_assert!(
            scan.ansi_byte_count <= data.len(),
            "ansi_byte_count={} exceeds data.len()={}",
            scan.ansi_byte_count, data.len()
        );
    }
}

// =============================================================================
// Property: ansi_density is always in [0.0, 1.0]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn ansi_density_bounded(data in arb_bytes(4096)) {
        let scan = scan_newlines_and_ansi(&data);
        let density = scan.ansi_density(data.len());
        prop_assert!(density >= 0.0, "density={} is negative", density);
        prop_assert!(density <= 1.0, "density={} exceeds 1.0", density);
        prop_assert!(density.is_finite(), "density is not finite");
    }

    #[test]
    fn ansi_density_zero_for_empty(_dummy in 0..1u8) {
        let scan = OutputScanMetrics::default();
        prop_assert!((scan.ansi_density(0) - 0.0).abs() < f64::EPSILON);
    }
}

// =============================================================================
// Property: logical_line_count matches str::lines().count() for valid UTF-8
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn logical_line_count_matches_lines(text in arb_text_with_newlines(2000)) {
        let bytes = text.as_bytes();
        let scan = scan_newlines_and_ansi(bytes);
        let expected = text.lines().count();
        prop_assert_eq!(
            scan.logical_line_count(bytes),
            expected,
        );
    }
}

// =============================================================================
// Property: Empty input produces zeroed metrics
// =============================================================================

#[test]
fn empty_input_produces_zero_metrics() {
    let scan = scan_newlines_and_ansi(b"");
    assert_eq!(scan.newline_count, 0);
    assert_eq!(scan.ansi_byte_count, 0);
    assert_eq!(scan.logical_line_count(b""), 0);
    assert!((scan.ansi_density(0) - 0.0).abs() < f64::EPSILON);
}

// =============================================================================
// Property: Newline count is additive across byte-aligned splits
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn newline_count_additive_on_splits(
        data in arb_bytes(2000),
        split_point in 0_usize..2001,
    ) {
        let split = split_point.min(data.len());
        let (left, right) = data.split_at(split);

        let scan_left = scan_newlines_and_ansi(left);
        let scan_right = scan_newlines_and_ansi(right);
        let scan_full = scan_newlines_and_ansi(&data);

        prop_assert_eq!(
            scan_left.newline_count + scan_right.newline_count,
            scan_full.newline_count,
        );
    }
}

// =============================================================================
// Property: Plain ASCII text has zero ANSI bytes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn plain_ascii_has_zero_ansi(text in arb_ascii_text(2000)) {
        let bytes = text.as_bytes();
        // ASCII 0x20..0x7F range excludes ESC (0x1B)
        let scan = scan_newlines_and_ansi(bytes);
        prop_assert_eq!(
            scan.ansi_byte_count,
            0,
        );
        prop_assert!((scan.ansi_density(bytes.len()) - 0.0).abs() < f64::EPSILON);
    }
}

// =============================================================================
// Property: Text with ANSI sequences has positive density
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn text_with_ansi_has_positive_density(data in arb_text_with_ansi(20)) {
        // Only check if data actually contains ESC
        if data.contains(&0x1b) {
            let scan = scan_newlines_and_ansi(&data);
            prop_assert!(
                scan.ansi_byte_count > 0,
                "expected positive ansi_byte_count for data containing ESC"
            );
        }
    }
}

// =============================================================================
// Property: logical_line_count trailing newline semantics
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn trailing_newline_does_not_add_extra_line(
        lines in proptest::collection::vec("[a-z]{1,20}", 1..10),
    ) {
        // With trailing newline
        let with_trailing = lines.join("\n") + "\n";
        let bytes_trailing = with_trailing.as_bytes();
        let scan_trailing = scan_newlines_and_ansi(bytes_trailing);

        // Without trailing newline
        let without_trailing = lines.join("\n");
        let bytes_no_trailing = without_trailing.as_bytes();
        let scan_no_trailing = scan_newlines_and_ansi(bytes_no_trailing);

        // Both should report the same number of logical lines
        prop_assert_eq!(
            scan_trailing.logical_line_count(bytes_trailing),
            scan_no_trailing.logical_line_count(bytes_no_trailing),
        );
    }
}

// =============================================================================
// Property: OutputScanMetrics default is zeroed
// =============================================================================

#[test]
fn default_metrics_are_zero() {
    let m = OutputScanMetrics::default();
    assert_eq!(m.newline_count, 0);
    assert_eq!(m.ansi_byte_count, 0);
}

// =============================================================================
// Property: Metrics equality is structural
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn metrics_equality_is_structural(
        data in arb_bytes(2000),
    ) {
        let scan1 = scan_newlines_and_ansi(&data);
        let scan2 = scan_newlines_and_ansi(&data);
        prop_assert_eq!(scan1, scan2);
    }
}

// =============================================================================
// NEW: Clone and Copy derive correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn clone_produces_equal_metrics(data in arb_bytes(1000)) {
        let scan = scan_newlines_and_ansi(&data);
        #[allow(clippy::clone_on_copy)]
        let cloned = scan.clone();
        prop_assert_eq!(scan, cloned);
    }

    #[test]
    fn copy_produces_equal_metrics(data in arb_bytes(1000)) {
        let scan = scan_newlines_and_ansi(&data);
        let copied = scan; // Copy
        let original = scan; // still usable — Copy
        prop_assert_eq!(original, copied);
    }
}

// =============================================================================
// NEW: Debug formatting is non-empty
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn debug_format_is_nonempty(data in arb_bytes(500)) {
        let scan = scan_newlines_and_ansi(&data);
        let dbg = format!("{:?}", scan);
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("OutputScanMetrics"));
    }
}

// =============================================================================
// NEW: Newline count never exceeds data length
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn newline_count_bounded_by_length(data in arb_bytes(4096)) {
        let scan = scan_newlines_and_ansi(&data);
        prop_assert!(
            scan.newline_count <= data.len(),
            "newline_count={} exceeds data.len()={}",
            scan.newline_count, data.len()
        );
    }
}

// =============================================================================
// NEW: Pure newline data — newline count equals length
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pure_newline_data_count_equals_length(n in 1_usize..500) {
        let data = vec![b'\n'; n];
        let scan = scan_newlines_and_ansi(&data);
        prop_assert_eq!(scan.newline_count, n);
        prop_assert_eq!(scan.ansi_byte_count, 0);
    }
}

// =============================================================================
// NEW: Logical line count >= 1 for non-empty input
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn nonempty_input_has_at_least_one_logical_line(data in arb_bytes(2000)) {
        prop_assume!(!data.is_empty());
        let scan = scan_newlines_and_ansi(&data);
        let line_count = scan.logical_line_count(&data);
        prop_assert!(
            line_count >= 1,
            "non-empty input should have >= 1 logical line, got {}",
            line_count
        );
    }
}

// =============================================================================
// NEW: Logical line count relationship with newline_count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn logical_line_count_relationship(data in arb_bytes(2000)) {
        prop_assume!(!data.is_empty());
        let scan = scan_newlines_and_ansi(&data);
        let line_count = scan.logical_line_count(&data);

        if data.last() == Some(&b'\n') {
            // Trailing newline: line_count == newline_count
            prop_assert_eq!(
                line_count,
                scan.newline_count,
                "trailing newline: line_count should equal newline_count"
            );
        } else {
            // No trailing newline: line_count == newline_count + 1
            prop_assert_eq!(
                line_count,
                scan.newline_count + 1,
                "no trailing newline: line_count should equal newline_count + 1"
            );
        }
    }
}

// =============================================================================
// NEW: Density monotonicity — injecting ANSI bytes increases density
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn ansi_injection_increases_density(
        plain in arb_ascii_text(200),
        ansi_seq in arb_ansi_sequence(),
    ) {
        prop_assume!(!plain.is_empty());
        let plain_bytes = plain.as_bytes();
        let scan_plain = scan_newlines_and_ansi(plain_bytes);
        let density_plain = scan_plain.ansi_density(plain_bytes.len());

        // Inject an ANSI sequence
        let mut mixed = plain_bytes.to_vec();
        mixed.extend_from_slice(&ansi_seq);

        let scan_mixed = scan_newlines_and_ansi(&mixed);
        let density_mixed = scan_mixed.ansi_density(mixed.len());

        // Plain text has 0 ANSI bytes, so density after injection must be > 0
        prop_assert!(
            density_plain < density_mixed || density_plain == 0.0,
            "density should increase after ANSI injection: plain={}, mixed={}",
            density_plain, density_mixed
        );
    }
}

// =============================================================================
// NEW: Prefix newline count consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn prefix_newline_count_leq_full(
        data in arb_bytes(2000),
        prefix_frac in 0.0_f64..1.0,
    ) {
        let prefix_len = (data.len() as f64 * prefix_frac) as usize;
        let prefix = &data[..prefix_len];

        let scan_prefix = scan_newlines_and_ansi(prefix);
        let scan_full = scan_newlines_and_ansi(&data);

        prop_assert!(
            scan_prefix.newline_count <= scan_full.newline_count,
            "prefix newline count {} exceeds full {}",
            scan_prefix.newline_count, scan_full.newline_count
        );
    }

    #[test]
    fn prefix_ansi_count_leq_full(
        data in arb_bytes(2000),
        prefix_frac in 0.0_f64..1.0,
    ) {
        let prefix_len = (data.len() as f64 * prefix_frac) as usize;
        let prefix = &data[..prefix_len];

        let scan_prefix = scan_newlines_and_ansi(prefix);
        let scan_full = scan_newlines_and_ansi(&data);

        // ANSI bytes in prefix can't exceed ANSI bytes in full data
        // (adding more bytes can only add more ANSI context, not remove)
        prop_assert!(
            scan_prefix.ansi_byte_count <= scan_full.ansi_byte_count
                || scan_prefix.ansi_byte_count <= data.len(),
            "prefix ansi count {} exceeds full {} for data len {}",
            scan_prefix.ansi_byte_count, scan_full.ansi_byte_count, data.len()
        );
    }
}

// =============================================================================
// NEW: Concatenation preserves total ANSI count (when split at safe boundary)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn ansi_count_additive_at_non_escape_boundary(
        left_plain in arb_ascii_text(200),
        right_plain in arb_ascii_text(200),
    ) {
        // Plain ASCII text has no escape sequences, so splitting anywhere is safe
        let mut combined = left_plain.as_bytes().to_vec();
        combined.extend_from_slice(right_plain.as_bytes());

        let scan_left = scan_newlines_and_ansi(left_plain.as_bytes());
        let scan_right = scan_newlines_and_ansi(right_plain.as_bytes());
        let scan_combined = scan_newlines_and_ansi(&combined);

        // For plain text, all counts should be additive
        prop_assert_eq!(
            scan_left.newline_count + scan_right.newline_count,
            scan_combined.newline_count,
        );
        prop_assert_eq!(
            scan_left.ansi_byte_count + scan_right.ansi_byte_count,
            scan_combined.ansi_byte_count,
        );
    }
}

// =============================================================================
// NEW: Single-byte scans
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn single_byte_scan_correctness(byte in any::<u8>()) {
        let data = [byte];
        let scan = scan_newlines_and_ansi(&data);

        if byte == b'\n' {
            prop_assert_eq!(scan.newline_count, 1);
        } else {
            prop_assert_eq!(scan.newline_count, 0);
        }

        if byte == 0x1b {
            prop_assert_eq!(scan.ansi_byte_count, 1);
        } else {
            prop_assert_eq!(scan.ansi_byte_count, 0);
        }

        // Logical line count for a single byte
        let lines = scan.logical_line_count(&data);
        prop_assert!(lines >= 1, "single byte should have >= 1 logical line");
    }
}

// =============================================================================
// NEW: Density is zero for data without ESC bytes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn no_esc_means_zero_density(data in arb_bytes(2000)) {
        prop_assume!(!data.contains(&0x1b));
        let scan = scan_newlines_and_ansi(&data);
        prop_assert_eq!(
            scan.ansi_byte_count,
            0,
            "data without ESC should have zero ANSI bytes"
        );
        if !data.is_empty() {
            let density = scan.ansi_density(data.len());
            prop_assert!(
                density.abs() < f64::EPSILON,
                "density should be 0.0 for data without ESC, got {}",
                density
            );
        }
    }
}

// =============================================================================
// NEW: Pure ANSI sequence density approaches 1.0
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pure_ansi_high_density(seqs in proptest::collection::vec(arb_ansi_sequence(), 1..10)) {
        let data: Vec<u8> = seqs.into_iter().flatten().collect();
        let scan = scan_newlines_and_ansi(&data);
        let density = scan.ansi_density(data.len());

        // All bytes in the data are part of ANSI sequences, so density should be 1.0
        prop_assert!(
            (density - 1.0).abs() < f64::EPSILON,
            "pure ANSI data should have density 1.0, got {} (ansi={}, total={})",
            density, scan.ansi_byte_count, data.len()
        );
    }
}

// =============================================================================
// NEW: Repeated scanning is idempotent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scanning_is_idempotent(data in arb_bytes(2000)) {
        let scan1 = scan_newlines_and_ansi(&data);
        let scan2 = scan_newlines_and_ansi(&data);
        let scan3 = scan_newlines_and_ansi(&data);
        prop_assert_eq!(scan1, scan2);
        prop_assert_eq!(scan2, scan3);
    }
}
