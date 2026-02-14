//! SIMD-friendly output scanning primitives with scalar fallback.
//!
//! This module provides a high-throughput scan for:
//! - newline byte count (`\n`)
//! - ANSI escape byte count (ESC ... final-byte)
//!
//! The fast path uses `memchr` (which uses vectorized implementations on
//! mainstream targets). A scalar fallback is kept as the reference behavior.

/// Aggregated scan metrics for pane output bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputScanMetrics {
    /// Count of `\n` bytes.
    pub newline_count: usize,
    /// Count of bytes that belong to ANSI escape sequences.
    pub ansi_byte_count: usize,
}

impl OutputScanMetrics {
    /// Compute logical line count using `str::lines` semantics.
    ///
    /// This matches `text.lines().count()` for UTF-8 text where line endings are
    /// represented by `\n` (including `\r\n`).
    #[must_use]
    pub fn logical_line_count(self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }
        if bytes.last() == Some(&b'\n') {
            self.newline_count
        } else {
            self.newline_count + 1
        }
    }

    /// Compute ANSI density as a fraction in `[0, 1]`.
    #[must_use]
    pub fn ansi_density(self, total_bytes: usize) -> f64 {
        if total_bytes == 0 {
            return 0.0;
        }
        self.ansi_byte_count as f64 / total_bytes as f64
    }
}

/// Scan output bytes for newline and ANSI escape density metrics.
#[must_use]
pub fn scan_newlines_and_ansi(bytes: &[u8]) -> OutputScanMetrics {
    if prefer_fast_path() {
        scan_newlines_and_ansi_memchr(bytes)
    } else {
        scan_newlines_and_ansi_scalar(bytes)
    }
}

#[inline]
fn prefer_fast_path() -> bool {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64"))]
    {
        true
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// Hybrid scan: vectorized `memchr` for newline counting + scalar ANSI state
/// machine. This avoids the ANSI-heavy regression that a pure `memchr2` approach
/// suffers (dense escape sequences cause the gap-processing loop to scan nearly
/// every byte, adding vectorized-search overhead on top of the scalar work).
///
/// Trade-off: two passes over the data, but the `memchr` newline pass is so fast
/// (~5-6 GiB/s on aarch64) that it's negligible compared to the scalar pass.
#[must_use]
fn scan_newlines_and_ansi_memchr(bytes: &[u8]) -> OutputScanMetrics {
    // Pass 1: vectorized newline count (memchr uses NEON/SSE/AVX internally).
    let newline_count = memchr::memchr_iter(b'\n', bytes).count();

    // Pass 2: scalar ANSI state machine — sequential state tracking has no
    // known vectorization shortcut.
    let mut ansi_byte_count = 0usize;
    let mut in_escape = false;
    for &b in bytes {
        if b == 0x1b {
            in_escape = true;
            ansi_byte_count += 1;
        } else if in_escape {
            ansi_byte_count += 1;
            if (0x40..=0x7E).contains(&b) && b != b'[' {
                in_escape = false;
            }
        }
    }

    OutputScanMetrics {
        newline_count,
        ansi_byte_count,
    }
}

#[must_use]
pub(crate) fn scan_newlines_and_ansi_scalar(bytes: &[u8]) -> OutputScanMetrics {
    let mut newline_count = 0usize;
    let mut ansi_byte_count = 0usize;
    let mut in_escape = false;

    for &b in bytes {
        if b == b'\n' {
            newline_count += 1;
        }

        if b == 0x1b {
            in_escape = true;
            ansi_byte_count += 1;
        } else if in_escape {
            ansi_byte_count += 1;
            if (0x40..=0x7E).contains(&b) && b != b'[' {
                in_escape = false;
            }
        }
    }

    OutputScanMetrics {
        newline_count,
        ansi_byte_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn plain_text_has_zero_ansi_density() {
        let text = b"hello world";
        let scan = scan_newlines_and_ansi(text);
        assert_eq!(scan.ansi_byte_count, 0);
        assert!(scan.ansi_density(text.len()).abs() < f64::EPSILON);
    }

    #[test]
    fn csi_sequence_counts_ansi_bytes() {
        let text = b"\x1b[31mred\x1b[0m";
        let scan = scan_newlines_and_ansi(text);
        assert!(scan.ansi_byte_count > 0);
        assert!(scan.ansi_density(text.len()) > 0.0);
    }

    #[test]
    fn logical_line_count_matches_expected_cases() {
        let cases: [(&[u8], usize); 6] = [
            (b"", 0),
            (b"one", 1),
            (b"one\n", 1),
            (b"one\ntwo", 2),
            (b"one\ntwo\n", 2),
            (b"one\n\ntwo", 3),
        ];

        for (bytes, expected) in cases {
            let scan = scan_newlines_and_ansi(bytes);
            assert_eq!(scan.logical_line_count(bytes), expected);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn fast_path_matches_scalar_for_random_bytes(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            let fast = scan_newlines_and_ansi_memchr(&data);
            let scalar = scan_newlines_and_ansi_scalar(&data);
            prop_assert_eq!(fast, scalar);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn logical_line_count_matches_str_lines(text in ".{0,1024}") {
            let bytes = text.as_bytes();
            let scan = scan_newlines_and_ansi(bytes);
            prop_assert_eq!(scan.logical_line_count(bytes), text.lines().count());
        }
    }

    // -----------------------------------------------------------------------
    // Edge cases: empty and single-byte inputs
    // -----------------------------------------------------------------------

    #[test]
    fn empty_input_yields_zero_metrics() {
        let scan = scan_newlines_and_ansi(b"");
        assert_eq!(scan.newline_count, 0);
        assert_eq!(scan.ansi_byte_count, 0);
        assert_eq!(scan.logical_line_count(b""), 0);
        assert!(scan.ansi_density(0).abs() < f64::EPSILON);
    }

    #[test]
    fn single_newline() {
        let scan = scan_newlines_and_ansi(b"\n");
        assert_eq!(scan.newline_count, 1);
        assert_eq!(scan.ansi_byte_count, 0);
        // "\n".lines().count() == 0 but our semantic: trailing-\n means newline_count lines.
        assert_eq!(scan.logical_line_count(b"\n"), 1);
    }

    #[test]
    fn single_esc_byte_counted_as_ansi() {
        let scan = scan_newlines_and_ansi(b"\x1b");
        assert_eq!(scan.ansi_byte_count, 1);
        assert_eq!(scan.newline_count, 0);
    }

    // -----------------------------------------------------------------------
    // ANSI escape edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn incomplete_csi_at_end_stays_in_escape() {
        // ESC [ without a final byte — the entire sequence is counted as ANSI bytes.
        let data = b"hello\x1b[";
        let scan = scan_newlines_and_ansi(data);
        // ESC and '[' are both counted.
        assert_eq!(scan.ansi_byte_count, 2);
    }

    #[test]
    fn csi_with_parameters_counts_all_ansi_bytes() {
        // ESC [ 3 8 ; 5 ; 1 9 6 m  => CSI with extended color
        let data = b"\x1b[38;5;196m";
        let scan = scan_newlines_and_ansi(data);
        // All bytes from ESC to 'm' inclusive.
        assert_eq!(scan.ansi_byte_count, data.len());
    }

    #[test]
    fn back_to_back_escape_sequences() {
        // Two complete CSI sequences: ESC[1m (bold) + ESC[0m (reset)
        let data = b"\x1b[1m\x1b[0m";
        let scan = scan_newlines_and_ansi(data);
        // ESC[1m = 4 bytes, ESC[0m = 4 bytes
        assert_eq!(scan.ansi_byte_count, 8);
        assert_eq!(scan.newline_count, 0);
    }

    #[test]
    fn esc_followed_by_single_letter_is_two_byte_sequence() {
        // ESC M (reverse index) — single-char final byte.
        let data = b"\x1bM";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.ansi_byte_count, 2);
    }

    #[test]
    fn newline_inside_ansi_gap_counted_separately() {
        // Newline between two escape sequences.
        let data = b"\x1b[1m\nhello\x1b[0m";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.newline_count, 1);
        assert_eq!(scan.ansi_byte_count, 8); // 4 + 4
    }

    // -----------------------------------------------------------------------
    // Logical line count edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn all_newlines_input() {
        let data = b"\n\n\n\n\n";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.newline_count, 5);
        assert_eq!(scan.logical_line_count(data), 5);
    }

    #[test]
    fn text_without_trailing_newline_gets_extra_line() {
        let data = b"line1\nline2";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.newline_count, 1);
        assert_eq!(scan.logical_line_count(data), 2);
    }

    #[test]
    fn text_with_trailing_newline_no_extra_line() {
        let data = b"line1\nline2\n";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.newline_count, 2);
        assert_eq!(scan.logical_line_count(data), 2);
    }

    // -----------------------------------------------------------------------
    // ANSI density calculations
    // -----------------------------------------------------------------------

    #[test]
    fn all_ansi_input_density_is_one() {
        let data = b"\x1b[0m";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.ansi_byte_count, data.len());
        assert!((scan.ansi_density(data.len()) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ansi_density_mixed_content() {
        // "hi" + ESC[0m = 2 plain + 4 ANSI = 6 total, density = 4/6
        let data = b"hi\x1b[0m";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.ansi_byte_count, 4);
        let expected_density = 4.0 / 6.0;
        assert!((scan.ansi_density(data.len()) - expected_density).abs() < 1e-10);
    }

    // -----------------------------------------------------------------------
    // Scalar/memchr parity on crafted inputs
    // -----------------------------------------------------------------------

    #[test]
    fn scalar_and_memchr_agree_on_dense_ansi() {
        // Dense ANSI: alternating escape sequences and text
        let mut data = Vec::new();
        for i in 0..100 {
            data.extend_from_slice(b"\x1b[");
            data.extend_from_slice(format!("{i}").as_bytes());
            data.push(b'm');
            data.extend_from_slice(b"txt\n");
        }
        let fast = scan_newlines_and_ansi_memchr(&data);
        let scalar = scan_newlines_and_ansi_scalar(&data);
        assert_eq!(fast, scalar);
    }

    #[test]
    fn scalar_and_memchr_agree_on_binary_noise() {
        // All byte values 0..=255 repeated
        let data: Vec<u8> = (0..=255u8).collect();
        let fast = scan_newlines_and_ansi_memchr(&data);
        let scalar = scan_newlines_and_ansi_scalar(&data);
        assert_eq!(fast, scalar);
    }

    #[test]
    fn scalar_and_memchr_agree_on_only_esc_bytes() {
        let data = vec![0x1b; 256];
        let fast = scan_newlines_and_ansi_memchr(&data);
        let scalar = scan_newlines_and_ansi_scalar(&data);
        assert_eq!(fast, scalar);
        // Each ESC starts escape mode; next ESC restarts. All bytes are ANSI.
        assert_eq!(fast.ansi_byte_count, 256);
    }

    // -----------------------------------------------------------------------
    // OutputScanMetrics Default
    // -----------------------------------------------------------------------

    #[test]
    fn metrics_default_is_zero() {
        let m = OutputScanMetrics::default();
        assert_eq!(m.newline_count, 0);
        assert_eq!(m.ansi_byte_count, 0);
    }
}
