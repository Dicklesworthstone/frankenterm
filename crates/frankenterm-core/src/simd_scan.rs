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

/// Cross-chunk scan carry state.
///
/// This allows callers that process output in chunks to preserve parser state
/// when ANSI escapes or UTF-8 code points span chunk boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputScanState {
    /// True if previous chunk ended inside an ANSI escape sequence.
    pub in_escape: bool,
    /// Number of UTF-8 continuation bytes still expected.
    pub pending_utf8_continuations: u8,
}

impl OutputScanState {
    /// Whether the current chunk boundary splits a UTF-8 code point.
    #[must_use]
    pub fn has_partial_utf8(self) -> bool {
        self.pending_utf8_continuations > 0
    }

    /// Reset carry state.
    pub fn reset(&mut self) {
        *self = Self::default();
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

/// Scan output bytes while preserving ANSI/UTF-8 boundary state.
///
/// Use this for chunked processing where segment boundaries may cut through
/// ANSI escape sequences or UTF-8 code points.
#[must_use]
pub fn scan_newlines_and_ansi_with_state(
    bytes: &[u8],
    state: &mut OutputScanState,
) -> OutputScanMetrics {
    if prefer_fast_path() {
        scan_newlines_and_ansi_memchr_with_state(bytes, state)
    } else {
        scan_newlines_and_ansi_scalar_with_state(bytes, state)
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
/// machine from the first ESC onward.
///
/// For ESC-free inputs (common for plain logs), this short-circuits and returns
/// `ansi_byte_count = 0` without a scalar byte walk.
#[must_use]
fn scan_newlines_and_ansi_memchr(bytes: &[u8]) -> OutputScanMetrics {
    // Pass 1: vectorized newline count (memchr uses NEON/SSE/AVX internally).
    let newline_count = memchr::memchr_iter(b'\n', bytes).count();

    let Some(first_esc) = memchr::memchr(0x1b, bytes) else {
        return OutputScanMetrics {
            newline_count,
            ansi_byte_count: 0,
        };
    };

    // Pass 2: scalar ANSI state machine from the first ESC onward.
    let mut ansi_byte_count = 0usize;
    let mut in_escape = false;
    for &b in &bytes[first_esc..] {
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
fn scan_newlines_and_ansi_memchr_with_state(
    bytes: &[u8],
    state: &mut OutputScanState,
) -> OutputScanMetrics {
    // Keep newline count on the vectorized fast path.
    let newline_count = memchr::memchr_iter(b'\n', bytes).count();

    // Fast path for the dominant case: no escape carry and no ESC in this
    // chunk. We only need UTF-8 carry-state updates.
    if !state.in_escape && memchr::memchr(0x1b, bytes).is_none() {
        state.pending_utf8_continuations =
            scan_utf8_pending_only(bytes, state.pending_utf8_continuations);
        return OutputScanMetrics {
            newline_count,
            ansi_byte_count: 0,
        };
    }

    let mut ansi_byte_count = 0usize;
    let mut in_escape = state.in_escape;
    let mut pending_utf8 = state.pending_utf8_continuations;
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

        update_utf8_pending(&mut pending_utf8, b);
    }

    state.in_escape = in_escape;
    state.pending_utf8_continuations = pending_utf8;

    OutputScanMetrics {
        newline_count,
        ansi_byte_count,
    }
}

#[inline]
fn scan_utf8_pending_only(bytes: &[u8], initial_pending: u8) -> u8 {
    if initial_pending == 0 && bytes.is_ascii() {
        return 0;
    }

    let mut pending = initial_pending;
    for &b in bytes {
        update_utf8_pending(&mut pending, b);
    }
    pending
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

#[must_use]
fn scan_newlines_and_ansi_scalar_with_state(
    bytes: &[u8],
    state: &mut OutputScanState,
) -> OutputScanMetrics {
    let mut newline_count = 0usize;
    let mut ansi_byte_count = 0usize;
    let mut in_escape = state.in_escape;
    let mut pending_utf8 = state.pending_utf8_continuations;

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

        update_utf8_pending(&mut pending_utf8, b);
    }

    state.in_escape = in_escape;
    state.pending_utf8_continuations = pending_utf8;

    OutputScanMetrics {
        newline_count,
        ansi_byte_count,
    }
}

#[inline]
fn update_utf8_pending(pending: &mut u8, byte: u8) {
    if *pending == 0 {
        if byte < 0x80 {
            return;
        }

        *pending = match byte {
            0xC2..=0xDF => 1,
            0xE0..=0xEF => 2,
            0xF0..=0xF4 => 3,
            _ => 0,
        };
        return;
    }

    if *pending > 0 {
        if (byte & 0b1100_0000) == 0b1000_0000 {
            *pending -= 1;
            return;
        }
        // Invalid continuation - reset and treat this byte as a fresh lead byte.
        *pending = 0;
    }

    *pending = match byte {
        0xC2..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF4 => 3,
        _ => 0,
    };
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
        #![proptest_config(ProptestConfig::with_cases(200))]

        #[test]
        fn stateful_fast_path_matches_scalar_for_random_bytes_and_state(
            data in proptest::collection::vec(any::<u8>(), 0..4096),
            in_escape in any::<bool>(),
            pending in 0_u8..4,
        ) {
            let initial = OutputScanState {
                in_escape,
                pending_utf8_continuations: pending,
            };
            let mut fast_state = initial;
            let mut scalar_state = initial;

            let fast = scan_newlines_and_ansi_memchr_with_state(&data, &mut fast_state);
            let scalar = scan_newlines_and_ansi_scalar_with_state(&data, &mut scalar_state);

            prop_assert_eq!(fast, scalar);
            prop_assert_eq!(fast_state, scalar_state);
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

    #[test]
    fn metrics_clone_and_copy() {
        let m = OutputScanMetrics {
            newline_count: 5,
            ansi_byte_count: 10,
        };
        let m2 = m;
        let m3 = m;
        assert_eq!(m, m2);
        assert_eq!(m, m3);
    }

    #[test]
    fn metrics_debug_format() {
        let m = OutputScanMetrics {
            newline_count: 3,
            ansi_byte_count: 7,
        };
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("3"));
        assert!(dbg.contains("7"));
    }

    #[test]
    fn crlf_counted_as_single_newline() {
        let data = b"line1\r\nline2\r\n";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.newline_count, 2);
    }

    #[test]
    fn osc_sequence_esc_bracket_stays_in_escape() {
        // ESC ] is OSC opener — '[' not involved so final byte check triggers on ']'.
        // Actually ESC ] starts OSC but ']' has value 0x5D which is in 0x40..=0x7E
        // and is not '[', so this terminates immediately.
        let data = b"\x1b]";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.ansi_byte_count, 2);
    }

    #[test]
    fn large_plain_text_no_ansi() {
        let data = "hello world\n".repeat(1000);
        let scan = scan_newlines_and_ansi(data.as_bytes());
        assert_eq!(scan.newline_count, 1000);
        assert_eq!(scan.ansi_byte_count, 0);
        assert!(scan.ansi_density(data.len()).abs() < f64::EPSILON);
    }

    #[test]
    fn mixed_esc_and_newlines_interleaved() {
        // Pattern: ESC[Am\n repeated — each CSI is 4 bytes
        let mut data = Vec::new();
        for _ in 0..50 {
            data.extend_from_slice(b"\x1b[Am\n");
        }
        let scan = scan_newlines_and_ansi(&data);
        assert_eq!(scan.newline_count, 50);
        // Each \x1b[Am is: ESC(1) + [(1, stays in escape since '[' excluded) + A(1, terminates) = wait
        // Actually: ESC starts escape. '[' is next — 0x5B is in 0x40..0x7E but IS '[' so excluded from termination.
        // Then 'A' is 0x41, in range and not '[', terminates. So 3 bytes per sequence.
        // But 'm' is not in escape anymore. Wait let me re-check.
        // \x1b[Am: ESC=0x1b, [=0x5b, A=0x41, m=0x6d
        // ESC -> in_escape=true, ansi_byte_count=1
        // [ -> in_escape, 0x5B in 0x40..=0x7E but == '[', so no termination. ansi_byte_count=2
        // A -> in_escape, 0x41 in 0x40..=0x7E and != '[', terminates. ansi_byte_count=3
        // m -> not in escape, not ESC. Not counted.
        // So 3 ANSI bytes per sequence, 50 sequences = 150.
        assert_eq!(scan.ansi_byte_count, 150);
    }

    #[test]
    fn ansi_density_half() {
        // 4 ANSI bytes + 4 plain bytes = density 0.5
        let data = b"abcd\x1b[0m";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.ansi_byte_count, 4);
        assert!((scan.ansi_density(data.len()) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn logical_line_count_single_char_no_newline() {
        let data = b"x";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.logical_line_count(data), 1);
    }

    #[test]
    fn newline_in_middle_of_csi_params() {
        // Newline inside CSI parameters: ESC[1\n0m
        let data = b"\x1b[1\n0m";
        let scan = scan_newlines_and_ansi(data);
        assert_eq!(scan.newline_count, 1);
        // ESC, [, 1 are ANSI (3 bytes). Then \n is 0x0A, not in 0x40..=0x7E, stays in escape.
        // 0 is 0x30, not in range, stays. m is 0x6D, in range, terminates.
        // So ANSI bytes: ESC, [, 1, \n, 0, m = 6
        assert_eq!(scan.ansi_byte_count, 6);
    }

    #[test]
    fn stateful_scan_tracks_escape_across_chunk_boundary() {
        let full = b"\x1b[31mred\x1b[0m";
        let mut state = OutputScanState::default();

        let left = scan_newlines_and_ansi_with_state(b"\x1b[31", &mut state);
        assert_eq!(left.ansi_byte_count, 4);
        assert!(state.in_escape);
        assert!(!state.has_partial_utf8());

        let right = scan_newlines_and_ansi_with_state(b"mred\x1b[0m", &mut state);
        assert_eq!(right.ansi_byte_count, 5);
        assert!(!state.in_escape);
        assert!(!state.has_partial_utf8());

        let stitched = OutputScanMetrics {
            newline_count: left.newline_count + right.newline_count,
            ansi_byte_count: left.ansi_byte_count + right.ansi_byte_count,
        };
        assert_eq!(stitched, scan_newlines_and_ansi(full));
    }

    #[test]
    fn stateful_scan_tracks_partial_utf8_across_chunks() {
        let mut state = OutputScanState::default();
        let left = b"ok\xf0\x9f";
        let right = b"\x99\x82\n";

        let left_scan = scan_newlines_and_ansi_with_state(left, &mut state);
        assert_eq!(left_scan.newline_count, 0);
        assert!(state.has_partial_utf8());
        assert_eq!(state.pending_utf8_continuations, 2);

        let right_scan = scan_newlines_and_ansi_with_state(right, &mut state);
        assert_eq!(right_scan.newline_count, 1);
        assert!(!state.has_partial_utf8());

        let stitched = OutputScanMetrics {
            newline_count: left_scan.newline_count + right_scan.newline_count,
            ansi_byte_count: left_scan.ansi_byte_count + right_scan.ansi_byte_count,
        };
        assert_eq!(stitched, scan_newlines_and_ansi(b"ok\xf0\x9f\x99\x82\n"));
    }

    #[test]
    fn stateful_esc_free_fast_path_matches_scalar_with_pending_utf8() {
        let data = b"plain ascii log line\nanother line";
        let initial = OutputScanState {
            in_escape: false,
            pending_utf8_continuations: 1,
        };

        let mut fast_state = initial;
        let mut scalar_state = initial;

        let fast = scan_newlines_and_ansi_memchr_with_state(data, &mut fast_state);
        let scalar = scan_newlines_and_ansi_scalar_with_state(data, &mut scalar_state);

        assert_eq!(fast, scalar);
        assert_eq!(fast_state, scalar_state);
    }

    #[test]
    fn stateful_esc_free_ascii_clears_invalid_utf8_pending() {
        let mut state = OutputScanState {
            in_escape: false,
            pending_utf8_continuations: 2,
        };

        let _ = scan_newlines_and_ansi_with_state(b"A", &mut state);
        assert_eq!(state.pending_utf8_continuations, 0);
        assert!(!state.in_escape);
    }
}
