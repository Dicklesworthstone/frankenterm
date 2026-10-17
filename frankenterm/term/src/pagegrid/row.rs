//! The row header (PageGrid ADR section 3.2).

/// A row header word. The row's cells are the block at `slot * (cols + 1)`;
/// its seqno is the page's seqno word for `slot`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct RowHeader(u64);

impl RowHeader {
    const SLOT_MASK: u64 = 0xFFFF_FFFF;
    const LEN_SHIFT: u32 = 32;
    const LEN_MASK: u64 = 0x1_FFFF << 32;

    /// Renderer consume-and-clear.
    pub const DIRTY: u64 = 1 << 49;
    /// Summary: the row may hold rich cells.
    pub const STYLED: u64 = 1 << 50;
    /// Summary: the row may hold grapheme cells.
    pub const GRAPHEME: u64 = 1 << 51;
    /// Summary: the row may hold hyperlink cells.
    pub const HYPERLINK: u64 = 1 << 52;
    /// Summary: the row may hold image cells.
    pub const IMAGE: u64 = 1 << 53;
    /// Summary: the row may hold cells that are not Output.
    pub const SEMANTIC: u64 = 1 << 54;
    pub const BIDI_ENABLED: u64 = 1 << 55;
    pub const RTL: u64 = 1 << 56;
    pub const AUTO_DETECT_DIRECTION: u64 = 1 << 57;
    pub const DOUBLE_WIDTH: u64 = 1 << 58;
    pub const DOUBLE_HEIGHT_TOP: u64 = 1 << 59;
    pub const DOUBLE_HEIGHT_BOTTOM: u64 = 1 << 60;
    /// Whether legacy would hold this row in clustered storage (ADR
    /// section 9); stored, not interpreted, by the page.
    pub const LEGACY_FORM_C: u64 = 1 << 61;
    pub const RESERVED: u64 = 0b11 << 62;

    /// The summary flags, which may be stale-true but never stale-false (I9).
    pub const SUMMARY: u64 =
        Self::STYLED | Self::GRAPHEME | Self::HYPERLINK | Self::IMAGE | Self::SEMANTIC;
    /// The flags mirrored from legacy `LineBits`.
    pub const LINE_FLAGS: u64 = Self::BIDI_ENABLED
        | Self::RTL
        | Self::AUTO_DETECT_DIRECTION
        | Self::DOUBLE_WIDTH
        | Self::DOUBLE_HEIGHT_TOP
        | Self::DOUBLE_HEIGHT_BOTTOM
        | Self::LEGACY_FORM_C;

    /// An empty row whose cells are the block at `slot`.
    pub const fn with_slot(slot: u32) -> Self {
        Self(slot as u64)
    }

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub fn slot(self) -> u32 {
        (self.0 & Self::SLOT_MASK) as u32
    }

    /// Legacy `Line::len()`: 0..=cols+1.
    pub fn len(self) -> usize {
        ((self.0 & Self::LEN_MASK) >> Self::LEN_SHIFT) as usize
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn has(self, flag: u64) -> bool {
        self.0 & flag != 0
    }

    pub fn with_len(self, len: usize) -> Self {
        debug_assert!(len <= (Self::LEN_MASK >> Self::LEN_SHIFT) as usize);
        Self((self.0 & !Self::LEN_MASK) | (((len as u64) << Self::LEN_SHIFT) & Self::LEN_MASK))
    }

    pub fn with_flags(self, flags: u64, on: bool) -> Self {
        debug_assert_eq!(
            flags & (Self::SLOT_MASK | Self::LEN_MASK | Self::RESERVED),
            0
        );
        if on {
            Self(self.0 | flags)
        } else {
            Self(self.0 & !flags)
        }
    }
}
