//! Crash-safe scrollback file format (v1).
//!
//! **Bead:** [BR-TERM-EMULATOR-UPLIFT-2.5.1] / `ft-kscfg`.
//! **Design doc:** [`docs/design/crash-safe-scrollback.md`]
//! (../../../../docs/design/crash-safe-scrollback.md).
//! **Parent epic:** ft-2okh0.5.
//!
//! # What this module ships
//!
//! The pure-format substrate for the per-pane crash-safe scrollback
//! file at `~/.local/share/ft/scrollback/<pane_uuid>.bin`:
//!
//! - [`FormatVersion`] — current is `V1`.
//! - [`HeaderFlags`] — bit-packed flags (encryption, etc.).
//! - [`ScrollbackHeader`] — the 256-byte fixed header.
//! - [`RecordKind`] — tagged-length record types in the ring.
//! - [`RecordHeader`] — `u32 record_len + u8 record_kind +
//!   3 reserved bytes` per ring entry.
//! - Round-trip serde via [`ScrollbackHeader::encode`] /
//!   [`ScrollbackHeader::decode`] and matching record helpers.
//!
//! # What this module does NOT ship (filed as follow-on)
//!
//! - Actual mmap + msync wiring into the ingest pipeline. The
//!   bead's full acceptance ("mmap-backed write path lands in the
//!   ingest pipeline") is a separate, larger ship that consumes
//!   the format types defined here. Filed as ft-kscfg.cont when
//!   this commit lands.
//! - File path management + flock advisory + per-pane size cap
//!   enforcement (also part of the wiring follow-on).
//! - Encryption layer (feature-gated, deferred to ft-kscfg.crypto).
//!
//! # Why ship the format first
//!
//! The five sub-beads of ft-2okh0.5 all consume the same on-disk
//! shape. Pinning the format types in their own module — with
//! exhaustive round-trip tests — means each sub-bead implements
//! against a stable contract. ft-5te6x (recovery protocol) reads
//! the same header layout that ft-kscfg writes; ft-0ulxc (kill-9
//! fixture) asserts byte-for-byte equality of pre-msync bytes
//! against this format.

use std::convert::TryInto;

/// Magic bytes at the start of every scrollback file. Protects
/// against accidental mmap of an unrelated file at the same path.
pub const MAGIC: &[u8; 4] = b"FTSB";

/// Total size of [`ScrollbackHeader`] on disk. Page-aligned for
/// mmap convenience and 4-cache-line wide so atomic updates to
/// the write cursor + last-msync timestamp don't straddle cache
/// lines.
pub const HEADER_SIZE: usize = 256;

/// Size of [`RecordHeader`] (the per-record tag prefix in the ring buffer).
/// The 8-byte layout is `u32 record_len + u8 record_kind + 3 reserved bytes`.
/// The reserved bytes preserve future tagging room without a format-version
/// bump.
pub const RECORD_HEADER_SIZE: usize = 8;

/// File-format version. Bump on incompatible changes; the
/// recovery flow refuses to mount an unknown version.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatVersion {
    /// v1 — initial format defined in `docs/design/crash-safe-scrollback.md`.
    V1 = 1,
}

impl FormatVersion {
    /// Convert from the on-disk u16. Returns `None` on unknown
    /// versions so the caller can refuse to mount the file.
    #[must_use]
    pub fn from_u16(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::V1),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_u16(self) -> u16 {
        self as u16
    }
}

/// Bit-packed header flags.
///
/// `bit 0` (mask `0x0001`): file is encrypted at rest. The body
/// (everything after [`HEADER_SIZE`]) is AES-256-GCM ciphertext,
/// keyed by `~/.local/share/ft/scrollback.key`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeaderFlags(u16);

impl HeaderFlags {
    /// `0x0001` — body is AES-256-GCM ciphertext.
    pub const ENCRYPTED: Self = Self(0x0001);

    #[must_use]
    pub fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn from_bits(raw: u16) -> Self {
        Self(raw)
    }

    #[must_use]
    pub fn bits(self) -> u16 {
        self.0
    }

    #[must_use]
    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    #[must_use]
    pub fn with(mut self, other: Self) -> Self {
        self.0 |= other.0;
        self
    }
}

/// Tagged-length record kinds in the ring buffer. The on-disk
/// `u8 record_kind` field is a discriminant from this enum.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    /// Plain text bytes appended to the pane.
    Text = 1,
    /// OSC (Operating System Command) escape sequence.
    Osc = 2,
    /// CSI (Control Sequence Introducer) escape sequence.
    Csi = 3,
    /// Cursor movement / position update.
    Cursor = 4,
    /// Clear-screen / clear-region command.
    Clear = 5,
}

impl RecordKind {
    #[must_use]
    pub fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Text),
            2 => Some(Self::Osc),
            3 => Some(Self::Csi),
            4 => Some(Self::Cursor),
            5 => Some(Self::Clear),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The 256-byte fixed header at the start of every scrollback file.
/// Layout pinned by `docs/design/crash-safe-scrollback.md`:
///
/// ```text
///  byte offset    field                        notes
///  0..4           "FTSB"                       magic
///  4..6           u16  format_version          starts at 1
///  6..8           u16  flags                   bit 0 = encrypted
///  8..16          u64  capacity_bytes          ring size; page-aligned
/// 16..24          u64  write_cursor_bytes      offset into the ring
/// 24..56          [u8; 32]  pane_uuid          binary form
/// 56..64          u64  created_at_epoch_ms
/// 64..72          u64  last_msync_at_epoch_ms  last successful msync
/// 72..80          u64  redactions_applied      counter
/// 80..88          u64  total_bytes_written     monotone; for histograms
/// 88..256         [u8; 168]  reserved          zero-filled
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbackHeader {
    pub version: FormatVersion,
    pub flags: HeaderFlags,
    pub capacity_bytes: u64,
    pub write_cursor_bytes: u64,
    pub pane_uuid: [u8; 32],
    pub created_at_epoch_ms: u64,
    pub last_msync_at_epoch_ms: u64,
    pub redactions_applied: u64,
    pub total_bytes_written: u64,
}

impl ScrollbackHeader {
    /// Construct a fresh header for a newly-created file.
    #[must_use]
    pub fn new(pane_uuid: [u8; 32], capacity_bytes: u64, created_at_epoch_ms: u64) -> Self {
        Self {
            version: FormatVersion::V1,
            flags: HeaderFlags::empty(),
            capacity_bytes,
            write_cursor_bytes: 0,
            pane_uuid,
            created_at_epoch_ms,
            last_msync_at_epoch_ms: 0,
            redactions_applied: 0,
            total_bytes_written: 0,
        }
    }

    /// Encode the header into a fixed-size 256-byte buffer.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0..4].copy_from_slice(MAGIC);
        buf[4..6].copy_from_slice(&self.version.as_u16().to_le_bytes());
        buf[6..8].copy_from_slice(&self.flags.bits().to_le_bytes());
        buf[8..16].copy_from_slice(&self.capacity_bytes.to_le_bytes());
        buf[16..24].copy_from_slice(&self.write_cursor_bytes.to_le_bytes());
        buf[24..56].copy_from_slice(&self.pane_uuid);
        buf[56..64].copy_from_slice(&self.created_at_epoch_ms.to_le_bytes());
        buf[64..72].copy_from_slice(&self.last_msync_at_epoch_ms.to_le_bytes());
        buf[72..80].copy_from_slice(&self.redactions_applied.to_le_bytes());
        buf[80..88].copy_from_slice(&self.total_bytes_written.to_le_bytes());
        // 88..256 stays zero-filled (reserved).
        buf
    }

    /// Decode a header from the on-disk bytes. Returns `Err` for
    /// every condition that should make the recovery flow refuse
    /// to mount the file (bad magic, unknown version, oversize
    /// cursor, etc.).
    pub fn decode(bytes: &[u8]) -> Result<Self, HeaderDecodeError> {
        if bytes.len() < HEADER_SIZE {
            return Err(HeaderDecodeError::Truncated {
                expected: HEADER_SIZE,
                actual: bytes.len(),
            });
        }
        if &bytes[0..4] != MAGIC {
            return Err(HeaderDecodeError::BadMagic {
                observed: [bytes[0], bytes[1], bytes[2], bytes[3]],
            });
        }
        let version_raw = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
        let version =
            FormatVersion::from_u16(version_raw).ok_or(HeaderDecodeError::UnknownVersion {
                observed: version_raw,
            })?;
        let flags = HeaderFlags::from_bits(u16::from_le_bytes(bytes[6..8].try_into().unwrap()));
        let capacity_bytes = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let write_cursor_bytes = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        if write_cursor_bytes > capacity_bytes {
            return Err(HeaderDecodeError::CursorBeyondCapacity {
                cursor: write_cursor_bytes,
                capacity: capacity_bytes,
            });
        }
        let mut pane_uuid = [0u8; 32];
        pane_uuid.copy_from_slice(&bytes[24..56]);
        let created_at_epoch_ms = u64::from_le_bytes(bytes[56..64].try_into().unwrap());
        let last_msync_at_epoch_ms = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        let redactions_applied = u64::from_le_bytes(bytes[72..80].try_into().unwrap());
        let total_bytes_written = u64::from_le_bytes(bytes[80..88].try_into().unwrap());
        Ok(Self {
            version,
            flags,
            capacity_bytes,
            write_cursor_bytes,
            pane_uuid,
            created_at_epoch_ms,
            last_msync_at_epoch_ms,
            redactions_applied,
            total_bytes_written,
        })
    }
}

/// Failures the [`ScrollbackHeader::decode`] flow surfaces. Each
/// variant maps to a recovery-flow refusal — the file is left
/// in place (so the operator can inspect it) and surfaced as an
/// orphan with the specific reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderDecodeError {
    /// The file is shorter than [`HEADER_SIZE`].
    Truncated { expected: usize, actual: usize },
    /// Bad magic bytes — file is not a scrollback file.
    BadMagic { observed: [u8; 4] },
    /// Format version is unknown to this build.
    UnknownVersion { observed: u16 },
    /// The persisted write cursor exceeds the file's capacity —
    /// indicates corruption (e.g. a partial write that bumped the
    /// cursor before flushing).
    CursorBeyondCapacity { cursor: u64, capacity: u64 },
}

impl std::fmt::Display for HeaderDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { expected, actual } => {
                write!(
                    f,
                    "scrollback header truncated: expected {expected} bytes, got {actual}"
                )
            }
            Self::BadMagic { observed } => {
                write!(
                    f,
                    "scrollback header bad magic: expected {:?} got {:?}",
                    MAGIC, observed
                )
            }
            Self::UnknownVersion { observed } => {
                write!(f, "scrollback header unknown format version: {observed}")
            }
            Self::CursorBeyondCapacity { cursor, capacity } => {
                write!(
                    f,
                    "scrollback header write cursor {cursor} exceeds capacity {capacity}"
                )
            }
        }
    }
}

impl std::error::Error for HeaderDecodeError {}

/// Per-record tag prefix in the ring buffer. 8 bytes total:
/// 4-byte payload length + 1-byte record kind + 3 reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    /// Length of the record's payload (does NOT include this
    /// 8-byte header).
    pub record_len: u32,
    pub record_kind: RecordKind,
}

impl RecordHeader {
    #[must_use]
    pub fn encode(&self) -> [u8; RECORD_HEADER_SIZE] {
        let mut buf = [0u8; RECORD_HEADER_SIZE];
        buf[0..4].copy_from_slice(&self.record_len.to_le_bytes());
        buf[4] = self.record_kind.as_u8();
        // bytes 5..8 reserved (zero-filled).
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RecordDecodeError> {
        if bytes.len() < RECORD_HEADER_SIZE {
            return Err(RecordDecodeError::Truncated {
                expected: RECORD_HEADER_SIZE,
                actual: bytes.len(),
            });
        }
        let record_len = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        let record_kind = RecordKind::from_u8(bytes[4])
            .ok_or(RecordDecodeError::UnknownKind { observed: bytes[4] })?;
        Ok(Self {
            record_len,
            record_kind,
        })
    }
}

/// Failures from [`RecordHeader::decode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordDecodeError {
    Truncated { expected: usize, actual: usize },
    UnknownKind { observed: u8 },
}

impl std::fmt::Display for RecordDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated { expected, actual } => {
                write!(
                    f,
                    "record header truncated: expected {expected} bytes, got {actual}"
                )
            }
            Self::UnknownKind { observed } => {
                write!(f, "record header unknown record kind: {observed}")
            }
        }
    }
}

impl std::error::Error for RecordDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> ScrollbackHeader {
        ScrollbackHeader {
            version: FormatVersion::V1,
            flags: HeaderFlags::ENCRYPTED,
            capacity_bytes: 50 * 1024 * 1024,
            write_cursor_bytes: 1_234_567,
            pane_uuid: [0x42; 32],
            created_at_epoch_ms: 1_715_000_000_000,
            last_msync_at_epoch_ms: 1_715_000_001_000,
            redactions_applied: 7,
            total_bytes_written: 9_876_543,
        }
    }

    #[test]
    fn header_size_is_256_bytes() {
        let bytes = sample_header().encode();
        assert_eq!(bytes.len(), HEADER_SIZE);
        assert_eq!(HEADER_SIZE, 256);
    }

    #[test]
    fn header_round_trip_preserves_every_field() {
        let h = sample_header();
        let bytes = h.encode();
        let decoded = ScrollbackHeader::decode(&bytes).expect("round-trip");
        assert_eq!(h, decoded);
    }

    #[test]
    fn header_decode_truncated_input_errors() {
        let err = ScrollbackHeader::decode(&[0u8; 100]).unwrap_err();
        assert!(matches!(err, HeaderDecodeError::Truncated { .. }));
    }

    #[test]
    fn header_decode_bad_magic_errors() {
        let mut bytes = sample_header().encode();
        bytes[0] = b'X';
        let err = ScrollbackHeader::decode(&bytes).unwrap_err();
        assert!(matches!(err, HeaderDecodeError::BadMagic { .. }));
    }

    #[test]
    fn header_decode_unknown_version_errors() {
        let mut bytes = sample_header().encode();
        // Bump version to an unknown value (e.g. 999).
        bytes[4..6].copy_from_slice(&999_u16.to_le_bytes());
        let err = ScrollbackHeader::decode(&bytes).unwrap_err();
        assert!(matches!(
            err,
            HeaderDecodeError::UnknownVersion { observed: 999 }
        ));
    }

    #[test]
    fn header_decode_cursor_beyond_capacity_errors() {
        let mut h = sample_header();
        h.write_cursor_bytes = h.capacity_bytes + 1;
        let bytes = h.encode();
        let err = ScrollbackHeader::decode(&bytes).unwrap_err();
        assert!(matches!(
            err,
            HeaderDecodeError::CursorBeyondCapacity { .. }
        ));
    }

    #[test]
    fn header_reserved_region_is_zero_filled() {
        let bytes = sample_header().encode();
        // bytes 88..256 must all be zero.
        for (offset, b) in bytes[88..].iter().enumerate() {
            assert_eq!(
                *b,
                0,
                "reserved byte {} must be zero, got {b:#x}",
                offset + 88
            );
        }
    }

    #[test]
    fn header_flags_contains_encryption_bit() {
        let f = HeaderFlags::ENCRYPTED;
        assert!(f.contains(HeaderFlags::ENCRYPTED));
        assert_eq!(f.bits(), 0x0001);
    }

    #[test]
    fn header_flags_with_combines_bits() {
        let f = HeaderFlags::empty().with(HeaderFlags::ENCRYPTED);
        assert!(f.contains(HeaderFlags::ENCRYPTED));
    }

    #[test]
    fn record_header_size_is_8_bytes() {
        let h = RecordHeader {
            record_len: 42,
            record_kind: RecordKind::Text,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), RECORD_HEADER_SIZE);
        assert_eq!(RECORD_HEADER_SIZE, 8);
    }

    #[test]
    fn record_header_round_trip_preserves_fields() {
        for &kind in &[
            RecordKind::Text,
            RecordKind::Osc,
            RecordKind::Csi,
            RecordKind::Cursor,
            RecordKind::Clear,
        ] {
            let h = RecordHeader {
                record_len: 1234,
                record_kind: kind,
            };
            let bytes = h.encode();
            let decoded = RecordHeader::decode(&bytes).expect("round-trip");
            assert_eq!(h, decoded);
        }
    }

    #[test]
    fn record_header_decode_truncated_errors() {
        let err = RecordHeader::decode(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, RecordDecodeError::Truncated { .. }));
    }

    #[test]
    fn record_header_decode_unknown_kind_errors() {
        // record_len = 1, record_kind = 99 (unknown)
        let mut bytes = [0u8; RECORD_HEADER_SIZE];
        bytes[0..4].copy_from_slice(&1_u32.to_le_bytes());
        bytes[4] = 99;
        let err = RecordHeader::decode(&bytes).unwrap_err();
        assert!(matches!(
            err,
            RecordDecodeError::UnknownKind { observed: 99 }
        ));
    }

    #[test]
    fn record_header_reserved_bytes_are_zero() {
        let h = RecordHeader {
            record_len: 1,
            record_kind: RecordKind::Text,
        };
        let bytes = h.encode();
        // bytes 5..8 must all be zero.
        assert_eq!(bytes[5], 0);
        assert_eq!(bytes[6], 0);
        assert_eq!(bytes[7], 0);
    }

    #[test]
    fn format_version_round_trip() {
        assert_eq!(FormatVersion::from_u16(1), Some(FormatVersion::V1));
        assert_eq!(FormatVersion::V1.as_u16(), 1);
        assert_eq!(FormatVersion::from_u16(0), None);
        assert_eq!(FormatVersion::from_u16(99), None);
    }

    #[test]
    fn record_kind_round_trip_every_variant() {
        for &kind in &[
            RecordKind::Text,
            RecordKind::Osc,
            RecordKind::Csi,
            RecordKind::Cursor,
            RecordKind::Clear,
        ] {
            let raw = kind.as_u8();
            assert_eq!(RecordKind::from_u8(raw), Some(kind));
        }
        assert_eq!(RecordKind::from_u8(0), None);
        assert_eq!(RecordKind::from_u8(99), None);
    }

    #[test]
    fn header_decode_preserves_pane_uuid_byte_for_byte() {
        let mut h = sample_header();
        for (i, byte) in h.pane_uuid.iter_mut().enumerate() {
            *byte = i as u8;
        }
        let bytes = h.encode();
        let decoded = ScrollbackHeader::decode(&bytes).unwrap();
        for i in 0..32 {
            assert_eq!(decoded.pane_uuid[i], i as u8);
        }
    }
}
