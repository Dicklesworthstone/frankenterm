//! The ground-state printable-ASCII scan (ft-yccm0.3.2.2).
//!
//! In ground state the parser hands printable text to the handler as whole
//! runs instead of feeding the state machine byte by byte. A run's ASCII
//! stretches are found here, as the length of the longest prefix of bytes in
//! `0x20..=0x7e`. The first byte outside that range ends the stretch: ESC
//! and the other C0 controls, DEL, or a byte >= 0x80 (UTF-8, which the
//! scalar run scanner takes over).
//!
//! [`AsciiScan::Scalar`] checks one byte at a time; it is the oracle the
//! `std::simd` scans are tested against, and what `FT_PARSER_SIMD=0`
//! selects. The `std::simd` scans compare 16, 32 or 64 bytes at once, with
//! a single unsigned compare per lane: shifted down by 0x20 (wrapping),
//! printable ASCII is exactly the bytes below 0x5f. Controls wrap to the top
//! of the range, and DEL and the bytes >= 0x80 land at 0x5f or above. The
//! first set bit of the comparison mask is the end of the stretch.
//!
//! `std::simd` needs the nightly `portable_simd` feature (the toolchain is
//! pinned to nightly). The `core::arch` NEON intrinsics would need `unsafe`,
//! which these crates forbid. On Apple Silicon the 16-byte scan is one NEON
//! register; the wider scans are for measuring.

use core::simd::Simd;
use core::simd::cmp::SimdPartialOrd;

/// How the ground-state scan finds the end of a printable-ASCII stretch.
/// Every variant gives the same answer; they differ only in speed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsciiScan {
    /// One byte at a time: the oracle, and the `FT_PARSER_SIMD=0` kill
    /// switch.
    Scalar,
    /// `std::simd`, 16 bytes at a time: the default.
    Simd16,
    /// `std::simd`, 32 bytes at a time.
    Simd32,
    /// `std::simd`, 64 bytes at a time.
    Simd64,
}

impl AsciiScan {
    /// What a parser uses unless `FT_PARSER_SIMD` says otherwise.
    pub const DEFAULT: AsciiScan = AsciiScan::Simd16;

    /// Every variant, for tests that hold them to the oracle.
    pub const ALL: [AsciiScan; 4] = [
        AsciiScan::Scalar,
        AsciiScan::Simd16,
        AsciiScan::Simd32,
        AsciiScan::Simd64,
    ];

    /// The `FT_PARSER_SIMD` policy: unset selects [`AsciiScan::DEFAULT`],
    /// a falsey value (`0`, `false`, `off`, `no`, empty) selects
    /// [`AsciiScan::Scalar`], and `16`, `32` or `64` select that width. Any
    /// other value selects the default.
    pub(super) fn for_env_value(value: Option<&str>) -> AsciiScan {
        let Some(value) = value else {
            return AsciiScan::DEFAULT;
        };
        if super::env_value_is_falsey(value) {
            return AsciiScan::Scalar;
        }
        match value.trim() {
            "16" => AsciiScan::Simd16,
            "32" => AsciiScan::Simd32,
            "64" => AsciiScan::Simd64,
            _ => AsciiScan::DEFAULT,
        }
    }

    /// The length of the longest prefix of `bytes` in `0x20..=0x7e`.
    #[inline]
    pub fn printable_len(self, bytes: &[u8]) -> usize {
        match self {
            AsciiScan::Scalar => printable_len_scalar(bytes),
            AsciiScan::Simd16 => printable_len_simd::<16>(bytes),
            AsciiScan::Simd32 => printable_len_simd::<32>(bytes),
            AsciiScan::Simd64 => printable_len_simd::<64>(bytes),
        }
    }
}

#[inline]
pub(super) fn is_printable_ascii(byte: u8) -> bool {
    matches!(byte, 0x20..=0x7e)
}

/// The oracle: one byte at a time.
#[inline]
fn printable_len_scalar(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|&byte| !is_printable_ascii(byte))
        .unwrap_or(bytes.len())
}

/// `LANES` bytes at a time while that many remain, then the oracle for the
/// tail.
#[inline]
fn printable_len_simd<const LANES: usize>(bytes: &[u8]) -> usize {
    let first_unprintable = Simd::<u8, LANES>::splat(0x5f);
    let space = Simd::<u8, LANES>::splat(0x20);
    let mut offset = 0;
    while let Some(chunk) = bytes.get(offset..offset + LANES) {
        let lanes = Simd::<u8, LANES>::from_slice(chunk);
        let stops = (lanes - space).simd_ge(first_unprintable).to_bitmask();
        if stops != 0 {
            return offset + stops.trailing_zeros() as usize;
        }
        offset += LANES;
    }
    offset + printable_len_scalar(&bytes[offset..])
}

#[cfg(all(test, feature = "std"))]
mod test {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn every_byte_value_stops_exactly_when_it_is_not_printable_ascii() {
        for byte in 0..=u8::MAX {
            for position in [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100] {
                let mut bytes = vec![b'a'; 130];
                bytes[position] = byte;
                let expected = if is_printable_ascii(byte) {
                    130
                } else {
                    position
                };
                for scan in AsciiScan::ALL {
                    assert_eq!(
                        scan.printable_len(&bytes),
                        expected,
                        "{scan:?} byte {byte:#04x} at {position}"
                    );
                }
            }
        }
    }

    #[test]
    fn short_and_empty_inputs_use_the_tail() {
        for scan in AsciiScan::ALL {
            assert_eq!(scan.printable_len(b""), 0);
            assert_eq!(scan.printable_len(b"a"), 1);
            assert_eq!(scan.printable_len(b"\x1b"), 0);
            assert_eq!(scan.printable_len(b"ab\n"), 2);
            assert_eq!(scan.printable_len(&[b'x'; 63]), 63);
        }
    }

    #[test]
    fn env_policy() {
        assert_eq!(AsciiScan::for_env_value(None), AsciiScan::DEFAULT);
        for falsey in ["0", "false", "OFF", " no ", ""] {
            assert_eq!(
                AsciiScan::for_env_value(Some(falsey)),
                AsciiScan::Scalar,
                "{falsey:?}"
            );
        }
        assert_eq!(AsciiScan::for_env_value(Some("16")), AsciiScan::Simd16);
        assert_eq!(AsciiScan::for_env_value(Some(" 32 ")), AsciiScan::Simd32);
        assert_eq!(AsciiScan::for_env_value(Some("64")), AsciiScan::Simd64);
        assert_eq!(AsciiScan::for_env_value(Some("1")), AsciiScan::DEFAULT);
        assert_eq!(AsciiScan::for_env_value(Some("on")), AsciiScan::DEFAULT);
    }

    /// Mostly printable ASCII, so stretches are long, with every other byte
    /// value mixed in.
    fn arb_byte() -> impl Strategy<Value = u8> {
        prop_oneof![
            30 => 0x20u8..=0x7e,
            1 => any::<u8>(),
        ]
    }

    proptest! {
        /// ft-yccm0.3.2.2: the `std::simd` scans agree with the scalar
        /// oracle on random byte vectors, at every offset.
        #[test]
        fn simd_scans_agree_with_the_scalar_oracle(
            bytes in proptest::collection::vec(arb_byte(), 0..300),
            start in 0usize..300,
        ) {
            let bytes = &bytes[start.min(bytes.len())..];
            let expected = printable_len_scalar(bytes);
            for scan in AsciiScan::ALL {
                prop_assert_eq!(scan.printable_len(bytes), expected, "{:?}", scan);
            }
        }
    }
}
