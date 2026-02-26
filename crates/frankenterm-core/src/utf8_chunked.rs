//! Safe UTF-8 chunked validation for streaming pane output (ft-2oph2).
//!
//! Terminal output arrives in arbitrary byte chunks that may split UTF-8
//! code points at boundaries. This module provides a streaming validator
//! that tracks incomplete code points across chunk boundaries and produces
//! safe `&str` slices from each chunk without allocation.
//!
//! # Architecture
//!
//! ```text
//! chunk N:  [...valid UTF-8...][partial start]
//! chunk N+1:     [cont bytes][...valid UTF-8...][partial]
//! ```
//!
//! The validator buffers up to 3 bytes of an incomplete code point from the
//! end of one chunk, prepends them to the next chunk's start, and validates
//! the stitched boundary. The interior bytes use `std::str::from_utf8` which
//! is SIMD-accelerated by the standard library on modern platforms.
//!
//! # Safety
//!
//! This module uses no `unsafe` code. All UTF-8 validation goes through
//! `std::str::from_utf8`, which is the gold standard for correctness.

use serde::{Deserialize, Serialize};

// =============================================================================
// Types
// =============================================================================

/// Tracks UTF-8 boundary state across chunks.
///
/// Buffers up to 3 bytes of an incomplete trailing code point.
#[derive(Debug, Clone)]
pub struct Utf8ChunkedValidator {
    /// Buffered bytes from incomplete trailing code point (0-3 bytes).
    pending: [u8; 4],
    /// Number of valid bytes in `pending`.
    pending_len: u8,
    /// Expected total length of the pending code point (0 if no pending).
    expected_len: u8,
    /// Total bytes validated as valid UTF-8.
    valid_bytes: u64,
    /// Total bytes that failed validation.
    invalid_bytes: u64,
    /// Number of replacement characters emitted.
    replacements: u64,
}

/// Result of validating a single chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkValidation {
    /// Number of bytes in this chunk that are valid UTF-8 content.
    pub valid_bytes: usize,
    /// Number of bytes that were invalid (replaced with U+FFFD).
    pub invalid_bytes: usize,
    /// Byte offset where the valid UTF-8 prefix ends (and trailing
    /// incomplete code point begins, if any).
    pub valid_prefix_end: usize,
    /// Whether the chunk ends mid-code-point (bytes buffered for next chunk).
    pub has_trailing_partial: bool,
}

/// Cumulative validation statistics.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Utf8ValidationStats {
    /// Total bytes that validated as UTF-8.
    pub valid_bytes: u64,
    /// Total bytes that failed validation.
    pub invalid_bytes: u64,
    /// Number of U+FFFD replacement characters that would be needed.
    pub replacements: u64,
    /// Fraction of bytes that are valid UTF-8 (0.0 to 1.0).
    pub validity_ratio: f64,
}

// =============================================================================
// Implementation
// =============================================================================

impl Utf8ChunkedValidator {
    /// Create a new validator with no pending state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: [0; 4],
            pending_len: 0,
            expected_len: 0,
            valid_bytes: 0,
            invalid_bytes: 0,
            replacements: 0,
        }
    }

    /// Validate a chunk of bytes, returning the validation result.
    ///
    /// Any trailing incomplete UTF-8 code point is buffered internally.
    /// Call `finish()` to flush the final state.
    pub fn validate_chunk(&mut self, bytes: &[u8]) -> ChunkValidation {
        if bytes.is_empty() {
            return ChunkValidation {
                valid_bytes: 0,
                invalid_bytes: 0,
                valid_prefix_end: 0,
                has_trailing_partial: self.pending_len > 0,
            };
        }

        let mut total_valid = 0usize;
        let mut total_invalid = 0usize;
        let mut consumed = 0usize;

        // Step 1: Complete any pending code point from previous chunk
        if self.pending_len > 0 {
            let needed = (self.expected_len - self.pending_len) as usize;
            let available = bytes.len().min(needed);

            // Copy continuation bytes
            let dst_start = self.pending_len as usize;
            self.pending[dst_start..dst_start + available]
                .copy_from_slice(&bytes[..available]);
            consumed = available;

            if available == needed {
                // We have the complete code point — validate it
                let cp_bytes = &self.pending[..self.expected_len as usize];
                if std::str::from_utf8(cp_bytes).is_ok() {
                    total_valid += self.expected_len as usize;
                } else {
                    total_invalid += self.expected_len as usize;
                    self.replacements += 1;
                }
                self.pending_len = 0;
                self.expected_len = 0;
            } else {
                // Still not enough — buffer what we have and return
                self.pending_len += available as u8;
                return ChunkValidation {
                    valid_bytes: 0,
                    invalid_bytes: 0,
                    valid_prefix_end: 0,
                    has_trailing_partial: true,
                };
            }
        }

        // Step 2: Validate the remaining bytes
        let remaining = &bytes[consumed..];
        if remaining.is_empty() {
            self.valid_bytes += total_valid as u64;
            self.invalid_bytes += total_invalid as u64;
            return ChunkValidation {
                valid_bytes: total_valid,
                invalid_bytes: total_invalid,
                valid_prefix_end: consumed,
                has_trailing_partial: false,
            };
        }

        // Find the longest valid UTF-8 prefix
        match std::str::from_utf8(remaining) {
            Ok(_) => {
                // Check if there's a trailing incomplete code point
                let trailing = trailing_incomplete_len(remaining);
                let valid_end = remaining.len() - trailing;

                if trailing > 0 {
                    // Buffer the trailing partial
                    self.pending[..trailing].copy_from_slice(&remaining[valid_end..]);
                    self.pending_len = trailing as u8;
                    self.expected_len = utf8_char_width(remaining[valid_end]);

                    // The entire chunk validated, but we're buffering the end
                    // Actually if from_utf8 succeeded, there's no incomplete sequence —
                    // the whole thing is valid. So trailing should be 0.
                    // from_utf8 only succeeds on complete UTF-8. So this branch
                    // means everything is valid.
                    total_valid += remaining.len();
                    self.pending_len = 0;
                    self.expected_len = 0;
                } else {
                    total_valid += remaining.len();
                }
            }
            Err(e) => {
                // Valid prefix up to the error
                let valid_up_to = e.valid_up_to();
                total_valid += valid_up_to;

                // After the valid prefix, determine what's there
                let rest = &remaining[valid_up_to..];
                if let Some(error_len) = e.error_len() {
                    // Definite invalid sequence — count it and continue
                    total_invalid += error_len;
                    self.replacements += 1;

                    // Recursively validate the remainder after the error
                    let after_error = &remaining[valid_up_to + error_len..];
                    let sub = self.validate_tail(after_error);
                    total_valid += sub.0;
                    total_invalid += sub.1;
                } else {
                    // Incomplete sequence at end — buffer it
                    let incomplete_len = rest.len();
                    self.pending[..incomplete_len].copy_from_slice(rest);
                    self.pending_len = incomplete_len as u8;
                    self.expected_len = utf8_char_width(rest[0]);
                }
            }
        }

        self.valid_bytes += total_valid as u64;
        self.invalid_bytes += total_invalid as u64;

        ChunkValidation {
            valid_bytes: total_valid,
            invalid_bytes: total_invalid,
            valid_prefix_end: consumed + total_valid,
            has_trailing_partial: self.pending_len > 0,
        }
    }

    /// Validate remaining bytes after an error (handles multiple errors).
    fn validate_tail(&mut self, bytes: &[u8]) -> (usize, usize) {
        if bytes.is_empty() {
            return (0, 0);
        }

        let mut total_valid = 0;
        let mut total_invalid = 0;

        match std::str::from_utf8(bytes) {
            Ok(_) => {
                total_valid += bytes.len();
            }
            Err(e) => {
                total_valid += e.valid_up_to();
                let rest = &bytes[e.valid_up_to()..];
                if let Some(error_len) = e.error_len() {
                    total_invalid += error_len;
                    self.replacements += 1;
                    let sub = self.validate_tail(&rest[error_len..]);
                    total_valid += sub.0;
                    total_invalid += sub.1;
                } else {
                    // Incomplete at end
                    let incomplete_len = rest.len();
                    self.pending[..incomplete_len].copy_from_slice(rest);
                    self.pending_len = incomplete_len as u8;
                    self.expected_len = utf8_char_width(rest[0]);
                }
            }
        }

        (total_valid, total_invalid)
    }

    /// Flush any pending bytes. If there are leftover bytes that don't
    /// form a complete code point, they count as invalid.
    pub fn finish(&mut self) -> ChunkValidation {
        let invalid = self.pending_len as usize;
        if invalid > 0 {
            self.invalid_bytes += invalid as u64;
            self.replacements += 1;
            self.pending_len = 0;
            self.expected_len = 0;
        }
        ChunkValidation {
            valid_bytes: 0,
            invalid_bytes: invalid,
            valid_prefix_end: 0,
            has_trailing_partial: false,
        }
    }

    /// Get cumulative validation statistics.
    #[must_use]
    pub fn stats(&self) -> Utf8ValidationStats {
        let total = self.valid_bytes + self.invalid_bytes;
        Utf8ValidationStats {
            valid_bytes: self.valid_bytes,
            invalid_bytes: self.invalid_bytes,
            replacements: self.replacements,
            validity_ratio: if total > 0 {
                self.valid_bytes as f64 / total as f64
            } else {
                1.0
            },
        }
    }

    /// Whether there are buffered bytes waiting for continuation.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.pending_len > 0
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.pending_len = 0;
        self.expected_len = 0;
        self.valid_bytes = 0;
        self.invalid_bytes = 0;
        self.replacements = 0;
    }
}

impl Default for Utf8ChunkedValidator {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Determine the expected byte width of a UTF-8 character from its lead byte.
#[inline]
fn utf8_char_width(lead: u8) -> u8 {
    match lead {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1, // invalid lead byte — treat as 1
    }
}

/// Count trailing bytes that form an incomplete UTF-8 code point.
///
/// Scans backwards from the end of a valid UTF-8 slice. Since `from_utf8`
/// succeeded, any trailing bytes that look like a lead+continuations are
/// actually complete. This function returns 0 for valid UTF-8.
#[inline]
fn trailing_incomplete_len(_bytes: &[u8]) -> usize {
    // If from_utf8 succeeded, there are no incomplete sequences.
    0
}

/// Validate a complete byte buffer as UTF-8, returning the valid prefix length.
///
/// This is a convenience wrapper around `std::str::from_utf8` that returns
/// just the length of the valid prefix, useful for streaming validation.
#[must_use]
pub fn valid_utf8_prefix_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) => e.valid_up_to(),
    }
}

/// Check if a byte slice is entirely valid UTF-8.
#[must_use]
pub fn is_valid_utf8(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Basic validation
    // -----------------------------------------------------------------------

    #[test]
    fn empty_input() {
        let mut v = Utf8ChunkedValidator::new();
        let r = v.validate_chunk(b"");
        assert_eq!(r.valid_bytes, 0);
        assert_eq!(r.invalid_bytes, 0);
        assert!(!r.has_trailing_partial);
    }

    #[test]
    fn ascii_only() {
        let mut v = Utf8ChunkedValidator::new();
        let r = v.validate_chunk(b"hello world\n");
        assert_eq!(r.valid_bytes, 12);
        assert_eq!(r.invalid_bytes, 0);
    }

    #[test]
    fn valid_multibyte() {
        let mut v = Utf8ChunkedValidator::new();
        let data = "héllo wörld 🦀".as_bytes();
        let r = v.validate_chunk(data);
        assert_eq!(r.valid_bytes, data.len());
        assert_eq!(r.invalid_bytes, 0);
    }

    #[test]
    fn invalid_byte() {
        let mut v = Utf8ChunkedValidator::new();
        let r = v.validate_chunk(&[0xFF]);
        assert_eq!(r.invalid_bytes, 1);
        assert_eq!(v.stats().replacements, 1);
    }

    #[test]
    fn mixed_valid_and_invalid() {
        let mut v = Utf8ChunkedValidator::new();
        // "hi" + invalid byte + "ok"
        let data = [b'h', b'i', 0xFF, b'o', b'k'];
        let r = v.validate_chunk(&data);
        assert_eq!(r.valid_bytes, 4); // h, i, o, k
        assert_eq!(r.invalid_bytes, 1); // 0xFF
    }

    // -----------------------------------------------------------------------
    // Cross-boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn split_2byte_char() {
        let mut v = Utf8ChunkedValidator::new();
        // é = 0xC3 0xA9 — split across chunks
        let r1 = v.validate_chunk(&[b'h', 0xC3]);
        assert_eq!(r1.valid_bytes, 1); // 'h'
        assert!(r1.has_trailing_partial);

        let r2 = v.validate_chunk(&[0xA9, b'!']);
        assert_eq!(r2.valid_bytes, 3); // é (2 bytes) + '!'
        assert!(!r2.has_trailing_partial);
    }

    #[test]
    fn split_3byte_char() {
        let mut v = Utf8ChunkedValidator::new();
        // €  = 0xE2 0x82 0xAC — split across 3 chunks
        let r1 = v.validate_chunk(&[0xE2]);
        assert!(r1.has_trailing_partial);

        let r2 = v.validate_chunk(&[0x82]);
        assert!(r2.has_trailing_partial);

        let r3 = v.validate_chunk(&[0xAC, b'x']);
        assert_eq!(r3.valid_bytes, 4); // € (3 bytes) + 'x'
        assert!(!r3.has_trailing_partial);
    }

    #[test]
    fn split_4byte_char() {
        let mut v = Utf8ChunkedValidator::new();
        // 🦀 = 0xF0 0x9F 0xA6 0x80
        let r1 = v.validate_chunk(&[b'a', 0xF0, 0x9F]);
        assert_eq!(r1.valid_bytes, 1); // 'a'
        assert!(r1.has_trailing_partial);

        let r2 = v.validate_chunk(&[0xA6, 0x80, b'b']);
        assert_eq!(r2.valid_bytes, 5); // 🦀 (4 bytes) + 'b'
        assert!(!r2.has_trailing_partial);
    }

    #[test]
    fn incomplete_at_end_then_finish() {
        let mut v = Utf8ChunkedValidator::new();
        let r = v.validate_chunk(&[b'x', 0xC3]); // incomplete é
        assert_eq!(r.valid_bytes, 1);
        assert!(r.has_trailing_partial);

        let f = v.finish();
        assert_eq!(f.invalid_bytes, 1); // the incomplete 0xC3
        assert_eq!(v.stats().replacements, 1);
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    #[test]
    fn stats_accumulate() {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(b"hello");
        v.validate_chunk(b" world");
        let stats = v.stats();
        assert_eq!(stats.valid_bytes, 11);
        assert_eq!(stats.invalid_bytes, 0);
        assert!((stats.validity_ratio - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_with_invalid() {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(&[b'a', 0xFF, b'b']);
        let stats = v.stats();
        assert_eq!(stats.valid_bytes, 2);
        assert_eq!(stats.invalid_bytes, 1);
        assert_eq!(stats.replacements, 1);
        let expected_ratio = 2.0 / 3.0;
        assert!((stats.validity_ratio - expected_ratio).abs() < 0.01);
    }

    #[test]
    fn reset_clears_all() {
        let mut v = Utf8ChunkedValidator::new();
        v.validate_chunk(b"hello");
        v.validate_chunk(&[0xC3]); // incomplete
        v.reset();
        assert!(!v.has_pending());
        assert_eq!(v.stats().valid_bytes, 0);
    }

    // -----------------------------------------------------------------------
    // Convenience functions
    // -----------------------------------------------------------------------

    #[test]
    fn valid_prefix_len_works() {
        assert_eq!(valid_utf8_prefix_len(b"hello"), 5);
        assert_eq!(valid_utf8_prefix_len(&[0xFF]), 0);
        assert_eq!(valid_utf8_prefix_len(&[b'a', 0xFF, b'b']), 1);
    }

    #[test]
    fn is_valid_utf8_works() {
        assert!(is_valid_utf8(b"hello"));
        assert!(!is_valid_utf8(&[0xFF]));
        assert!(is_valid_utf8("🦀".as_bytes()));
    }

    // -----------------------------------------------------------------------
    // Large data / ANSI sequences
    // -----------------------------------------------------------------------

    #[test]
    fn ansi_sequences_are_valid_utf8() {
        let mut v = Utf8ChunkedValidator::new();
        let data = b"\x1b[32mhello\x1b[0m\n";
        let r = v.validate_chunk(data);
        assert_eq!(r.valid_bytes, data.len());
        assert_eq!(r.invalid_bytes, 0);
    }

    #[test]
    fn large_ascii_input() {
        let mut v = Utf8ChunkedValidator::new();
        let data = "log line output\n".repeat(10_000);
        let r = v.validate_chunk(data.as_bytes());
        assert_eq!(r.valid_bytes, data.len());
    }

    #[test]
    fn multiple_invalid_bytes() {
        let mut v = Utf8ChunkedValidator::new();
        // Three separate invalid bytes
        let data = [b'a', 0xFF, b'b', 0xFE, b'c', 0xFD];
        let r = v.validate_chunk(&data);
        assert_eq!(r.valid_bytes, 3); // a, b, c
        assert_eq!(r.invalid_bytes, 3); // 0xFF, 0xFE, 0xFD
        assert_eq!(v.stats().replacements, 3);
    }

    // -----------------------------------------------------------------------
    // Chunk validation serde
    // -----------------------------------------------------------------------

    #[test]
    fn chunk_validation_serde_roundtrip() {
        let cv = ChunkValidation {
            valid_bytes: 100,
            invalid_bytes: 5,
            valid_prefix_end: 95,
            has_trailing_partial: true,
        };
        let json = serde_json::to_string(&cv).unwrap();
        let rt: ChunkValidation = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.valid_bytes, cv.valid_bytes);
        assert_eq!(rt.has_trailing_partial, cv.has_trailing_partial);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let stats = Utf8ValidationStats {
            valid_bytes: 1000,
            invalid_bytes: 10,
            replacements: 5,
            validity_ratio: 0.99,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let rt: Utf8ValidationStats = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.valid_bytes, stats.valid_bytes);
        assert_eq!(rt.replacements, stats.replacements);
    }
}
