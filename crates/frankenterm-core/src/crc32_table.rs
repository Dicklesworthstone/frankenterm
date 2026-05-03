//! Table-driven CRC32-IEEE (Sarwate 1988).
//!
//! Drop-in replacement for the bitwise reference impl in
//! `cell_consistency_crc::crc32_ieee` that uses a precomputed
//! 256-entry lookup table to process one byte per iteration with a
//! single XOR + table fetch, instead of 8 conditional XORs per byte.
//!
//! Output is bit-for-bit identical to the bitwise reference;
//! tests pin standard CRC32 vectors plus an equivalence sweep.
//!
//! Reference: Sarwate, Dilip V. (1988) "Computation of cyclic
//! redundancy checks via table look-up", Communications of the ACM
//! 31(8):1008-1013.
//!
//! ## Why a sibling module?
//!
//! `cell_consistency_crc.rs` documents the bitwise impl as a
//! deliberate dependency-free substrate, with the comment
//! "the integration layer can swap in a tabled version if hot-path
//! benchmarks demand it." This sibling module ships the tabled
//! version while preserving the dependency-free constraint
//! (single `static [u32; 256]` table generated at compile time).
//!
//! ## Speedup
//!
//! Per-byte cost drops from `8 × (cmp + xor + shr)` to
//! `1 × (xor + shr + load)` — typically 3-5× wall-clock on AArch64
//! and x86-64 in microbenchmarks. SIMD slice-by-N variants are
//! available for further speedup but introduce a kilobyte-class
//! table footprint; the single-table form here is the right
//! ergonomics/speed trade-off for cell-grid hashing where inputs
//! are small (~few KB) and the inner loop dominates.

const POLYNOMIAL_REVERSED: u32 = 0xedb8_8320;

/// Precomputed CRC32-IEEE lookup table.
///
/// `CRC32_TABLE[b]` is the CRC of the single byte `b` after the
/// initial 0xFFFF_FFFF state has been XORed in — i.e., the value
/// you XOR into the running register after consuming byte `b`.
const CRC32_TABLE: [u32; 256] = generate_crc32_table();

const fn generate_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i;
        let mut j = 0;
        while j < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLYNOMIAL_REVERSED & mask);
            j += 1;
        }
        table[i as usize] = crc;
        i += 1;
    }
    table
}

/// Compute the 32-bit CRC32-IEEE of a byte slice using the precomputed
/// 256-entry table. Output is identical to the bitwise reference impl
/// in `cell_consistency_crc::crc32_ieee`.
#[must_use]
pub fn crc32_ieee_tabled(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in bytes {
        crc = crc32_ieee_update_tabled(crc, byte);
    }
    !crc
}

#[must_use]
pub(crate) fn crc32_ieee_update_tabled(crc: u32, byte: u8) -> u32 {
    let index = ((crc ^ u32::from(byte)) & 0xff) as usize;
    (crc >> 8) ^ CRC32_TABLE[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation copied from cell_consistency_crc::crc32_ieee
    /// for equivalence testing without a cross-module dependency.
    fn crc32_ieee_bitwise(bytes: &[u8]) -> u32 {
        let mut crc: u32 = 0xffff_ffff;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    #[test]
    fn empty_input_yields_zero() {
        assert_eq!(crc32_ieee_tabled(b""), 0);
    }

    #[test]
    fn single_byte_zero_yields_known_value() {
        // CRC32-IEEE of a single 0x00 byte is 0xD202EF8D.
        assert_eq!(crc32_ieee_tabled(&[0u8]), 0xD202_EF8D);
    }

    #[test]
    fn classic_vector_quick_brown_fox() {
        // CRC32-IEEE of "The quick brown fox jumps over the lazy dog" is
        // 0x414FA339 (canonical zlib/IEEE checksum of this phrase).
        let input = b"The quick brown fox jumps over the lazy dog";
        assert_eq!(crc32_ieee_tabled(input), 0x414F_A339);
    }

    #[test]
    fn classic_vector_123456789() {
        // The most universally-cited CRC32 conformance vector:
        // crc32("123456789") = 0xCBF43926 (per ITU-T V.42, IEEE 802.3,
        // PNG, gzip, and every CRC reference in existence).
        assert_eq!(crc32_ieee_tabled(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn equivalence_with_bitwise_reference_on_diverse_inputs() {
        let inputs: &[&[u8]] = &[
            b"",
            b"a",
            b"ab",
            b"abcdefghij",
            b"\x00\x00\x00\x00",
            b"\xff\xff\xff\xff",
            b"\xde\xad\xbe\xef\xca\xfe\xba\xbe",
            b"The quick brown fox jumps over the lazy dog",
            b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
        ];
        for input in inputs {
            assert_eq!(
                crc32_ieee_tabled(input),
                crc32_ieee_bitwise(input),
                "mismatch on input {:?}",
                input
            );
        }
    }

    #[test]
    fn equivalence_on_pseudorandom_inputs() {
        // Linear-congruential generator gives reproducible bytes
        // without an rng dependency.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        for trial in 0..50 {
            let len = ((state >> 32) as usize) % 1024;
            let mut buf = vec![0u8; len];
            for byte in &mut buf {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *byte = (state >> 33) as u8;
            }
            assert_eq!(
                crc32_ieee_tabled(&buf),
                crc32_ieee_bitwise(&buf),
                "mismatch on pseudorandom trial {} (len={})",
                trial,
                len
            );
        }
    }

    #[test]
    fn table_first_and_last_entries_match_polynomial() {
        // Sanity-pin two table entries against hand-derivable values.
        // Entry 0: CRC of byte 0 (no flips) is 0.
        assert_eq!(CRC32_TABLE[0], 0);
        // Entry 1: CRC of byte 1 = 0xedb8_8320 >> 7 ... easier to just
        // assert it's nonzero and matches what bitwise would produce.
        let mut crc = 1u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (POLYNOMIAL_REVERSED & mask);
        }
        assert_eq!(CRC32_TABLE[1], crc);
    }
}
