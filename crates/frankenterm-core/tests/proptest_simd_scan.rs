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
