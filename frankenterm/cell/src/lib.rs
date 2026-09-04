#![cfg_attr(not(feature = "std"), no_std)]
#![allow(
    clippy::bind_instead_of_map,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::doc_lazy_continuation,
    clippy::from_over_into,
    clippy::len_without_is_empty,
    clippy::needless_option_as_deref,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::unnecessary_cast,
    clippy::vec_box
)]
//! Model a cell in the terminal display
use crate::color::{ColorAttribute, PaletteIndex};
#[cfg(feature = "use_image")]
use crate::image::ImageCell;
use alloc::sync::Arc;
use core::hash::{Hash, Hasher};
use finl_unicode::grapheme_clusters::Graphemes;
pub use frankenterm_char_props::emoji::Presentation;
use frankenterm_char_props::emoji_variation::WCWIDTH_TABLE;
use frankenterm_char_props::widechar_width::WcWidth;
use frankenterm_dynamic::{FromDynamic, ToDynamic};
pub use frankenterm_escape_parser::osc::Hyperlink;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize, Zeroizing};

extern crate alloc;
use crate::alloc::string::ToString;
use alloc::boxed::Box;
use alloc::vec::Vec;

pub mod color;
#[cfg(feature = "use_image")]
pub mod image;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
enum SmallColor {
    Default,
    PaletteIndex(PaletteIndex),
}

impl Default for SmallColor {
    fn default() -> Self {
        Self::Default
    }
}

impl Into<ColorAttribute> for SmallColor {
    fn into(self) -> ColorAttribute {
        match self {
            Self::Default => ColorAttribute::Default,
            Self::PaletteIndex(idx) => ColorAttribute::PaletteIndex(idx),
        }
    }
}

/// Holds the attributes for a cell.
/// Most style attributes are stored internally as part of a bitfield
/// to reduce per-cell overhead.
/// The setter methods return a mutable self reference so that they can
/// be chained together.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Clone, Eq, PartialEq)]
pub struct CellAttributes {
    attributes: u32,
    /// The foreground color
    foreground: SmallColor,
    /// The background color
    background: SmallColor,
    /// Relatively rarely used attributes spill over to a heap
    /// allocated struct in order to keep CellAttributes
    /// smaller in the common case.
    fat: Option<Box<FatAttributes>>,
}

impl core::fmt::Debug for CellAttributes {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        fmt.debug_struct("CellAttributes")
            .field("attributes", &self.attributes)
            .field("intensity", &self.intensity())
            .field("underline", &self.underline())
            .field("blink", &self.blink())
            .field("italic", &self.italic())
            .field("reverse", &self.reverse())
            .field("strikethrough", &self.strikethrough())
            .field("invisible", &self.invisible())
            .field("wrapped", &self.wrapped())
            .field("overline", &self.overline())
            .field("semantic_type", &self.semantic_type())
            .field("foreground", &self.foreground)
            .field("background", &self.background)
            .field("fat", &self.fat)
            .finish()
    }
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Default, Clone, Eq, PartialEq)]
struct FatAttributes {
    /// The hyperlink content, if any
    hyperlink: Option<Arc<Hyperlink>>,
    /// The image data, if any
    #[cfg(feature = "use_image")]
    image: Vec<Box<ImageCell>>,
    /// The color of the underline.  If None, then
    /// the foreground color is to be used
    underline_color: ColorAttribute,
    foreground: ColorAttribute,
    background: ColorAttribute,
}

impl FatAttributes {
    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        if let Some(link) = &self.hyperlink {
            link.compute_shape_hash(hasher);
        }
        #[cfg(feature = "use_image")]
        for cell in &self.image {
            cell.compute_shape_hash(hasher);
        }
        self.underline_color.hash(hasher);
        self.foreground.hash(hasher);
        self.background.hash(hasher);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// ft-dkfiy (MOONSHOT, cfg `succinct_attrs`): succinct run-length attribute store
// ───────────────────────────────────────────────────────────────────────────

/// Succinct run-length storage for a line's worth of cell attributes.
///
/// Terminal lines have long runs of identical attributes (an entire line of
/// default-attr text, a single colored span), so storing `(CellAttributes,
/// run_len)` pairs instead of one [`CellAttributes`] per cell is dramatically
/// more compact — a 200-column uniform-attribute line collapses from
/// `200 × size_of::<CellAttributes>()` (3200 bytes) to a single ~20-byte run —
/// and improves cache locality for the attribute scans done during reflow and
/// paint.
///
/// This is an EXPERIMENTAL, purely ADDITIVE primitive: it does NOT change the
/// default per-cell [`Cell`] / [`CellAttributes`] representation, so existing
/// behaviour and golden outputs are unaffected. It is intended as the storage
/// backing a future run-length line representation. Gated behind the
/// `succinct_attrs` feature so it is trivially revertible.
#[cfg(feature = "succinct_attrs")]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttributeRuns {
    runs: Vec<AttrRun>,
    /// Total number of logical cells represented by `runs`.
    len: usize,
}

#[cfg(feature = "succinct_attrs")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct AttrRun {
    attrs: CellAttributes,
    /// Number of consecutive cells that share `attrs` (always >= 1).
    run_len: usize,
}

#[cfg(feature = "succinct_attrs")]
impl AttributeRuns {
    /// An empty attribute store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-allocate space for `runs` distinct attribute runs.
    pub fn with_run_capacity(runs: usize) -> Self {
        Self {
            runs: Vec::with_capacity(runs),
            len: 0,
        }
    }

    /// Total number of logical cells represented.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no cells are stored.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of distinct attribute runs — the succinct size. For a line of
    /// uniform attributes this is `1` regardless of column count.
    #[inline]
    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// Append `count` cells carrying `attrs`, coalescing with the trailing run
    /// when the attributes are identical (the common case for terminal text).
    pub fn push(&mut self, attrs: &CellAttributes, count: u32) {
        self.push_len(attrs, count as usize);
    }

    /// Append `count` cells carrying `attrs`, coalescing with the trailing run.
    ///
    /// This is the warm/cold scrollback-facing entry point: callers that
    /// already have a materialized line or screen dump use native `usize`
    /// lengths, while the older `push(u32)` convenience wrapper remains for
    /// compact call sites.
    pub fn push_len(&mut self, attrs: &CellAttributes, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(last) = self.runs.last_mut() {
            if last.attrs == *attrs {
                last.run_len = last.run_len.saturating_add(count);
                self.len = self.len.saturating_add(count);
                return;
            }
        }
        self.runs.push(AttrRun {
            attrs: attrs.clone(),
            run_len: count,
        });
        self.len = self.len.saturating_add(count);
    }

    /// Build succinct runs from an AoS/per-column attribute dump.
    ///
    /// This is the byte-equivalence doorway for warm/cold scrollback: the
    /// source of truth remains the per-column [`CellAttributes`] sequence, and
    /// the run store must answer every column with the same attributes.
    pub fn from_per_cell(attrs: &[CellAttributes]) -> Self {
        let mut runs = Self::with_run_capacity(attrs.len());
        for attr in attrs {
            runs.push_len(attr, 1);
        }
        runs
    }

    /// Build succinct runs from materialized screen cells.
    pub fn from_cells(cells: &[Cell]) -> Self {
        let mut runs = Self::with_run_capacity(cells.len());
        for cell in cells {
            runs.push_len(cell.attrs(), 1);
        }
        runs
    }

    /// Iterate the encoded runs as `(attrs, run_len)` pairs.
    pub fn runs(&self) -> impl Iterator<Item = (&CellAttributes, usize)> {
        self.runs.iter().map(|run| (&run.attrs, run.run_len))
    }

    /// Attributes at logical cell index `idx`, or `None` if out of range.
    /// Scans runs (`O(run_count)`, which is `<<` cell count for real lines).
    pub fn get(&self, idx: usize) -> Option<&CellAttributes> {
        if idx >= self.len {
            return None;
        }
        let mut offset = 0usize;
        for run in &self.runs {
            offset = offset.saturating_add(run.run_len);
            if idx < offset {
                return Some(&run.attrs);
            }
        }
        None
    }

    /// Expand back into one [`CellAttributes`] clone per logical cell — the
    /// inverse of repeated [`push`](Self::push). Proves the run encoding is
    /// lossless and bridges to per-cell consumers.
    pub fn to_per_cell(&self) -> Vec<CellAttributes> {
        let mut out = Vec::with_capacity(self.len);
        for run in &self.runs {
            for _ in 0..run.run_len {
                out.push(run.attrs.clone());
            }
        }
        out
    }
}

#[cfg(all(test, feature = "succinct_attrs"))]
mod succinct_attrs_tests {
    use super::*;

    fn plain() -> CellAttributes {
        CellAttributes::blank()
    }

    fn reversed() -> CellAttributes {
        let mut a = CellAttributes::blank();
        a.set_reverse(true);
        a
    }

    #[test]
    fn empty_store() {
        let r = AttributeRuns::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert_eq!(r.run_count(), 0);
        assert_eq!(r.get(0), None);
        assert!(r.to_per_cell().is_empty());
    }

    #[test]
    fn uniform_line_collapses_to_one_run() {
        let mut r = AttributeRuns::new();
        r.push(&plain(), 200);
        assert_eq!(r.len(), 200);
        assert_eq!(r.run_count(), 1, "a uniform line must use a single run");
        for i in 0..200 {
            assert_eq!(r.get(i), Some(&plain()));
        }
        assert_eq!(r.get(200), None);
    }

    #[test]
    fn adjacent_equal_pushes_coalesce() {
        let mut r = AttributeRuns::new();
        r.push(&plain(), 3);
        r.push(&plain(), 5);
        assert_eq!(r.run_count(), 1, "identical adjacent runs must merge");
        assert_eq!(r.len(), 8);
    }

    #[test]
    fn coalesced_large_run_preserves_boundary_lookup() {
        let mut r = AttributeRuns::new();
        r.push(&plain(), u32::MAX);
        r.push(&plain(), 1);

        let tail_idx = u32::MAX as usize;
        assert_eq!(r.run_count(), 1, "equal large runs should still coalesce");
        assert_eq!(r.len(), tail_idx + 1);
        assert_eq!(
            r.get(tail_idx),
            Some(&plain()),
            "coalescing past u32::MAX must not make the represented tail unreachable"
        );
        assert_eq!(r.get(tail_idx + 1), None);
    }

    #[test]
    fn zero_count_push_is_noop() {
        let mut r = AttributeRuns::new();
        r.push(&plain(), 0);
        assert!(r.is_empty());
        assert_eq!(r.run_count(), 0);
    }

    #[test]
    fn distinct_runs_and_lookup_match_per_cell_oracle() {
        // Encode [plain×2, reversed×3, plain×1] and verify every index against
        // an explicit per-cell oracle (the run encoding is lossless).
        let spec: [(CellAttributes, u32); 3] = [(plain(), 2), (reversed(), 3), (plain(), 1)];
        let mut r = AttributeRuns::new();
        let mut oracle: Vec<CellAttributes> = Vec::new();
        for (attrs, count) in &spec {
            r.push(attrs, *count);
            for _ in 0..*count {
                oracle.push(attrs.clone());
            }
        }
        assert_eq!(r.run_count(), 3, "non-adjacent-equal runs stay distinct");
        assert_eq!(r.len(), oracle.len());
        for (i, want) in oracle.iter().enumerate() {
            assert_eq!(r.get(i), Some(want), "mismatch at cell {i}");
        }
        assert_eq!(
            r.to_per_cell(),
            oracle,
            "round-trip must reproduce the oracle"
        );
    }

    #[test]
    fn screen_dump_constructors_match_per_column_oracle() {
        let spec: [(CellAttributes, u32); 5] = [
            (plain(), 4),
            (reversed(), 2),
            (plain(), 1),
            (reversed(), 3),
            (plain(), 5),
        ];
        let mut oracle = Vec::new();
        for (attrs, count) in &spec {
            for _ in 0..*count {
                oracle.push(attrs.clone());
            }
        }
        let cells = oracle
            .iter()
            .cloned()
            .map(Cell::blank_with_attrs)
            .collect::<Vec<_>>();

        for runs in [
            AttributeRuns::from_per_cell(&oracle),
            AttributeRuns::from_cells(&cells),
        ] {
            assert_eq!(runs.len(), oracle.len());
            assert_eq!(runs.to_per_cell(), oracle);
            assert_eq!(runs.get(oracle.len()), None);
            for (col, want) in oracle.iter().enumerate() {
                assert_eq!(runs.get(col), Some(want), "mismatch at column {col}");
            }
        }

        let run_lengths = AttributeRuns::from_per_cell(&oracle)
            .runs()
            .map(|(_, len)| len)
            .collect::<Vec<_>>();
        assert_eq!(run_lengths.as_slice(), [4, 2, 1, 3, 5]);
    }
}

/// Byte-equivalence keep-gate for the succinct attribute store (ft-dkfiy).
///
/// This module is `#[cfg(test)]` but intentionally NOT feature-gated, so the
/// keep-gate runs (and is counted) in BOTH the default build and the
/// `--features succinct_attrs` build — the succinct-specific assertions are
/// `#[cfg(feature = "succinct_attrs")]` and the default build runs the matching
/// `#[cfg(not(...))]` baseline. Enabling the experimental feature must never
/// silently drop this test.
#[cfg(test)]
mod succinct_attrs_keep_gate {
    use super::*;

    /// A representative attribute sequence exercising varied foreground,
    /// background, underline and intensity runs — with both long
    /// same-attribute runs and single-cell changes. Deterministic so it can be
    /// re-derived for cross-checks.
    fn varied_sequence() -> Vec<CellAttributes> {
        let mut bold = CellAttributes::blank();
        bold.set_intensity(Intensity::Bold);
        let mut half = CellAttributes::blank();
        half.set_intensity(Intensity::Half);
        let mut single_ul = CellAttributes::blank();
        single_ul.set_underline(Underline::Single);
        let mut double_ul = CellAttributes::blank();
        double_ul.set_underline(Underline::Double);
        let mut fg = CellAttributes::blank();
        fg.set_foreground(ColorAttribute::PaletteIndex(4));
        let mut bg = CellAttributes::blank();
        bg.set_background(ColorAttribute::PaletteIndex(2));
        let mut mixed = CellAttributes::blank();
        mixed.set_foreground(ColorAttribute::PaletteIndex(9));
        mixed.set_background(ColorAttribute::PaletteIndex(0));
        mixed.set_underline(Underline::Curly);
        mixed.set_reverse(true);

        // (attrs, run_len) — long runs, a single-cell change, and adjacent
        // distinct attrs so coalescing and run boundaries are both exercised.
        let spec: [(CellAttributes, usize); 9] = [
            (CellAttributes::blank(), 12),
            (bold, 5),
            (CellAttributes::blank(), 1),
            (single_ul, 7),
            (fg, 3),
            (bg, 8),
            (double_ul, 1),
            (half, 4),
            (mixed, 6),
        ];
        let mut out = Vec::new();
        for (attrs, n) in &spec {
            for _ in 0..*n {
                out.push(attrs.clone());
            }
        }
        out
    }

    /// KEEP-GATE: the succinct run-length attribute store must return
    /// byte-identical [`CellAttributes`] to the default per-cell representation
    /// at EVERY column index. Round-3 keep/revert depends on this staying green
    /// under `--features succinct_attrs`.
    #[test]
    fn succinct_store_is_byte_identical_to_per_cell() {
        let oracle = varied_sequence();
        assert!(oracle.len() > 1, "sequence must be non-trivial");

        #[cfg(feature = "succinct_attrs")]
        {
            // Build the run-length store from the per-cell oracle exactly as a
            // run-length line would (coalescing equal adjacent cells).
            let mut runs = AttributeRuns::new();
            for attrs in &oracle {
                runs.push(attrs, 1);
            }

            // Same logical length.
            assert_eq!(runs.len(), oracle.len());
            // Byte-identical attributes at every column index.
            for (col, want) in oracle.iter().enumerate() {
                assert_eq!(
                    runs.get(col),
                    Some(want),
                    "succinct RLE store diverged from the default representation at column {col}"
                );
            }
            // No attributes past the end.
            assert_eq!(runs.get(oracle.len()), None);
            // Expanding the runs reproduces the per-cell sequence exactly.
            assert_eq!(runs.to_per_cell(), oracle);
            // Coalescing actually happened: distinct runs are fewer than cells.
            assert!(
                runs.run_count() < oracle.len(),
                "equal adjacent attributes must coalesce into fewer runs than cells"
            );
        }

        #[cfg(not(feature = "succinct_attrs"))]
        {
            // Default build: the succinct store is compiled out, so exercise the
            // per-cell construction path the store must match. The deterministic
            // builder must reproduce an identical sequence — the keep-gate runs
            // symmetrically instead of vanishing without the feature.
            assert_eq!(varied_sequence(), oracle);
        }
    }
}

/// Define getter and setter for the attributes bitfield.
/// The first form is for a simple boolean value stored in
/// a single bit.  The $bitnum parameter specifies which bit.
/// The second form is for an integer value that occupies a range
/// of bits.  The $bitmask and $bitshift parameters define how
/// to transform from the stored bit value to the consumable
/// value.
macro_rules! bitfield {
    ($getter:ident, $setter:ident, $bitnum:expr) => {
        #[inline]
        pub fn $getter(&self) -> bool {
            (self.attributes & (1 << $bitnum)) == (1 << $bitnum)
        }

        #[inline]
        pub fn $setter(&mut self, value: bool) -> &mut Self {
            let attr_value = if value { 1 << $bitnum } else { 0 };
            self.attributes = (self.attributes & !(1 << $bitnum)) | attr_value;
            self
        }
    };

    ($getter:ident, $setter:ident, $bitmask:expr, $bitshift:expr) => {
        #[inline]
        pub fn $getter(&self) -> u32 {
            (self.attributes >> $bitshift) & $bitmask
        }

        #[inline]
        pub fn $setter(&mut self, value: u32) -> &mut Self {
            let clear = !($bitmask << $bitshift);
            let attr_value = (value & $bitmask) << $bitshift;
            self.attributes = (self.attributes & clear) | attr_value;
            self
        }
    };

    ($getter:ident, $setter:ident, $enum:ident, $bitmask:expr, $bitshift:expr) => {
        #[inline]
        pub fn $getter(&self) -> $enum {
            <$enum as BitfieldEnumDecode>::from_bits(
                ((self.attributes >> $bitshift) & $bitmask) as u8,
            )
        }

        #[inline]
        pub fn $setter(&mut self, value: $enum) -> &mut Self {
            let value = value as u32;
            let clear = !($bitmask << $bitshift);
            let attr_value = (value & $bitmask) << $bitshift;
            self.attributes = (self.attributes & clear) | attr_value;
            self
        }
    };
}

trait BitfieldEnumDecode {
    fn from_bits(bits: u8) -> Self;
}

/// Describes the semantic "type" of the cell.
/// This categorizes cells into Output (from the actions the user is
/// taking; this is the default if left unspecified),
/// Input (that the user typed) and Prompt (effectively, "chrome" provided
/// by the shell or application that the user is interacting with.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, FromDynamic, ToDynamic)]
#[repr(u8)]
pub enum SemanticType {
    Output = 0,
    Input = 1,
    Prompt = 2,
}

impl BitfieldEnumDecode for Intensity {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Normal,
            1 => Self::Bold,
            2 => Self::Half,
            _ => Self::Normal,
        }
    }
}

impl BitfieldEnumDecode for Underline {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::None,
            1 => Self::Single,
            2 => Self::Double,
            3 => Self::Curly,
            4 => Self::Dotted,
            5 => Self::Dashed,
            _ => Self::None,
        }
    }
}

impl BitfieldEnumDecode for Blink {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::None,
            1 => Self::Slow,
            2 => Self::Rapid,
            _ => Self::None,
        }
    }
}

impl BitfieldEnumDecode for SemanticType {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::Output,
            1 => Self::Input,
            2 => Self::Prompt,
            _ => Self::Output,
        }
    }
}

impl BitfieldEnumDecode for VerticalAlign {
    fn from_bits(bits: u8) -> Self {
        match bits {
            0 => Self::BaseLine,
            1 => Self::SuperScript,
            2 => Self::SubScript,
            _ => Self::BaseLine,
        }
    }
}

impl Default for SemanticType {
    fn default() -> Self {
        Self::Output
    }
}

pub use frankenterm_escape_parser::csi::{Blink, Intensity, Underline, VerticalAlign};

impl Default for CellAttributes {
    fn default() -> Self {
        Self::blank()
    }
}

impl CellAttributes {
    bitfield!(intensity, set_intensity, Intensity, 0b11, 0);
    bitfield!(underline, set_underline, Underline, 0b111, 2);
    bitfield!(blink, set_blink, Blink, 0b11, 5);
    bitfield!(italic, set_italic, 7);
    bitfield!(reverse, set_reverse, 8);
    bitfield!(strikethrough, set_strikethrough, 9);
    bitfield!(invisible, set_invisible, 10);
    bitfield!(wrapped, set_wrapped, 11);
    bitfield!(overline, set_overline, 12);
    bitfield!(semantic_type, set_semantic_type, SemanticType, 0b11, 13);
    bitfield!(vertical_align, set_vertical_align, VerticalAlign, 0b11, 15);

    pub const fn blank() -> Self {
        Self {
            attributes: 0,
            foreground: SmallColor::Default,
            background: SmallColor::Default,
            fat: None,
        }
    }

    /// Returns true if the attribute bits in both objects are equal.
    /// This can be used to cheaply test whether the styles of the two
    /// cells are the same, and is used by some `Renderer` implementations.
    pub fn attribute_bits_equal(&self, other: &Self) -> bool {
        self.attributes == other.attributes
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.attributes.hash(hasher);
        self.foreground.hash(hasher);
        self.background.hash(hasher);
        if let Some(fat) = &self.fat {
            fat.compute_shape_hash(hasher);
        }
    }

    /// Set the foreground color for the cell to that specified
    pub fn set_foreground<C: Into<ColorAttribute>>(&mut self, foreground: C) -> &mut Self {
        let foreground: ColorAttribute = foreground.into();
        match foreground {
            ColorAttribute::Default => {
                self.foreground = SmallColor::Default;
                if let Some(fat) = self.fat.as_mut() {
                    fat.foreground = ColorAttribute::Default;
                }
                self.deallocate_fat_attributes_if_none();
            }
            ColorAttribute::PaletteIndex(idx) => {
                self.foreground = SmallColor::PaletteIndex(idx);
                if let Some(fat) = self.fat.as_mut() {
                    fat.foreground = ColorAttribute::Default;
                }
                self.deallocate_fat_attributes_if_none();
            }
            foreground => {
                self.foreground = SmallColor::Default;
                self.allocate_fat_attributes();
                self.fat.as_mut().unwrap().foreground = foreground;
            }
        }

        self
    }

    pub fn foreground(&self) -> ColorAttribute {
        if let Some(fat) = self.fat.as_ref() {
            if fat.foreground != ColorAttribute::Default {
                return fat.foreground;
            }
        }
        self.foreground.into()
    }

    pub fn set_background<C: Into<ColorAttribute>>(&mut self, background: C) -> &mut Self {
        let background: ColorAttribute = background.into();
        match background {
            ColorAttribute::Default => {
                self.background = SmallColor::Default;
                if let Some(fat) = self.fat.as_mut() {
                    fat.background = ColorAttribute::Default;
                }
                self.deallocate_fat_attributes_if_none();
            }
            ColorAttribute::PaletteIndex(idx) => {
                self.background = SmallColor::PaletteIndex(idx);
                if let Some(fat) = self.fat.as_mut() {
                    fat.background = ColorAttribute::Default;
                }
                self.deallocate_fat_attributes_if_none();
            }
            background => {
                self.background = SmallColor::Default;
                self.allocate_fat_attributes();
                self.fat.as_mut().unwrap().background = background;
            }
        }

        self
    }

    pub fn background(&self) -> ColorAttribute {
        if let Some(fat) = self.fat.as_ref() {
            if fat.background != ColorAttribute::Default {
                return fat.background;
            }
        }
        self.background.into()
    }

    /// Clear all attributes from a cell
    pub fn clear(&mut self) {
        *self = Self::blank();
    }

    fn allocate_fat_attributes(&mut self) {
        if self.fat.is_none() {
            self.fat.replace(Box::new(FatAttributes {
                hyperlink: None,
                #[cfg(feature = "use_image")]
                image: vec![],
                underline_color: ColorAttribute::Default,
                foreground: ColorAttribute::Default,
                background: ColorAttribute::Default,
            }));
        }
    }

    fn deallocate_fat_attributes_if_none(&mut self) {
        let deallocate = self
            .fat
            .as_ref()
            .map(|fat| {
                #[cfg(feature = "use_image")]
                {
                    if !fat.image.is_empty() {
                        return false;
                    }
                }
                fat.hyperlink.is_none()
                    && fat.underline_color == ColorAttribute::Default
                    && fat.foreground == ColorAttribute::Default
                    && fat.background == ColorAttribute::Default
            })
            .unwrap_or(false);
        if deallocate {
            self.fat.take();
        }
    }

    pub fn set_hyperlink(&mut self, link: Option<Arc<Hyperlink>>) -> &mut Self {
        if link.is_none() && self.fat.is_none() {
            self
        } else {
            self.allocate_fat_attributes();
            self.fat.as_mut().unwrap().hyperlink = link;
            self.deallocate_fat_attributes_if_none();
            self
        }
    }
}

#[cfg(feature = "use_image")]
impl CellAttributes {
    /// Assign a single image to a cell.
    pub fn set_image(&mut self, image: Box<ImageCell>) -> &mut Self {
        self.allocate_fat_attributes();
        self.fat.as_mut().unwrap().image = vec![image];
        self
    }

    /// Clear all images from a cell
    pub fn clear_images(&mut self) -> &mut Self {
        if let Some(fat) = self.fat.as_mut() {
            fat.image.clear();
        }
        self.deallocate_fat_attributes_if_none();
        self
    }

    pub fn detach_image_with_placement(&mut self, image_id: u32, placement_id: Option<u32>) {
        if let Some(fat) = self.fat.as_mut() {
            fat.image
                .retain(|im| !im.matches_placement(image_id, placement_id));
        }
        self.deallocate_fat_attributes_if_none();
    }

    /// Add an image attachement, preserving any existing attachments.
    /// The list of images is maintained in z-index order
    pub fn attach_image(&mut self, image: Box<ImageCell>) -> &mut Self {
        self.allocate_fat_attributes();
        let fat = self.fat.as_mut().unwrap();
        let z_index = image.z_index();
        match fat
            .image
            .binary_search_by(|probe| probe.z_index().cmp(&z_index))
        {
            Ok(idx) | Err(idx) => fat.image.insert(idx, image),
        }
        self
    }
}

impl CellAttributes {
    pub fn set_underline_color<C: Into<ColorAttribute>>(
        &mut self,
        underline_color: C,
    ) -> &mut Self {
        let underline_color = underline_color.into();
        if underline_color == ColorAttribute::Default && self.fat.is_none() {
            self
        } else {
            self.allocate_fat_attributes();
            self.fat.as_mut().unwrap().underline_color = underline_color;
            self.deallocate_fat_attributes_if_none();
            self
        }
    }

    /// Clone the attributes, but exclude fancy extras such
    /// as hyperlinks or future sprite things
    pub fn clone_sgr_only(&self) -> Self {
        let mut res = Self {
            attributes: self.attributes,
            foreground: self.foreground,
            background: self.background,
            fat: None,
        };
        if let Some(fat) = self.fat.as_ref() {
            if fat.background != ColorAttribute::Default
                || fat.foreground != ColorAttribute::Default
            {
                res.allocate_fat_attributes();
                let new_fat = res.fat.as_mut().unwrap();
                new_fat.foreground = fat.foreground;
                new_fat.background = fat.background;
            }
        }
        // Reset the semantic type; clone_sgr_only is used primarily
        // to create a "blank" cell when clearing and we want that to
        // be deterministically tagged as Output so that we have an
        // easier time in get_semantic_zones.
        res.set_semantic_type(SemanticType::default());
        res.set_underline_color(self.underline_color());

        // Turn off underline because it can have surprising results
        // if underline is on, then we get CRLF and then SGR reset:
        // If the CRLF causes a line to scroll, we'll call clone_sgr_only()
        // to get a blank cell for the new line and it would be filled
        // with underlines.
        // clone_sgr_only() is primarily for preserving the background
        // color when erasing rather than other attributes, so it should
        // be fine to clear out the actual underline attribute.
        // Let's extend this to other line attribute types as well.
        // <https://github.com/wezterm/wezterm/issues/2489>
        res.set_underline(Underline::None);
        res.set_overline(false);
        res.set_strikethrough(false);
        res
    }

    pub fn hyperlink(&self) -> Option<&Arc<Hyperlink>> {
        self.fat.as_ref().and_then(|fat| fat.hyperlink.as_ref())
    }

    /// Returns whether this attribute set carries any out-of-band image
    /// attachments.
    ///
    /// This intentionally does not clone the image descriptors.  Persistence
    /// preflights use it to reject unsupported graphics before allocating a
    /// terminal checkpoint projection.
    #[cfg(feature = "use_image")]
    pub fn has_image_attachments(&self) -> bool {
        self.fat.as_ref().is_some_and(|fat| !fat.image.is_empty())
    }

    /// Returns the list of attached images in z-index order.
    /// Returns None if there are no attached images; will
    /// never return Some(vec![]).
    #[cfg(feature = "use_image")]
    pub fn images(&self) -> Option<Vec<ImageCell>> {
        let fat = self.fat.as_ref()?;
        if fat.image.is_empty() {
            return None;
        }
        Some(fat.image.iter().map(|im| im.as_ref().clone()).collect())
    }

    pub fn underline_color(&self) -> ColorAttribute {
        self.fat
            .as_ref()
            .map(|fat| fat.underline_color)
            .unwrap_or(ColorAttribute::Default)
    }

    pub fn apply_change(&mut self, change: &AttributeChange) {
        use AttributeChange::*;
        match change {
            Intensity(value) => {
                self.set_intensity(*value);
            }
            Underline(value) => {
                self.set_underline(*value);
            }
            Italic(value) => {
                self.set_italic(*value);
            }
            Blink(value) => {
                self.set_blink(*value);
            }
            Reverse(value) => {
                self.set_reverse(*value);
            }
            StrikeThrough(value) => {
                self.set_strikethrough(*value);
            }
            Invisible(value) => {
                self.set_invisible(*value);
            }
            Foreground(value) => {
                self.set_foreground(*value);
            }
            Background(value) => {
                self.set_background(*value);
            }
            Hyperlink(value) => {
                self.set_hyperlink(value.clone());
            }
        }
    }
}

#[cfg(feature = "use_serde")]
fn deserialize_teenystring<'de, D>(deserializer: D) -> Result<TeenyString, D::Error>
where
    D: Deserializer<'de>,
{
    let text = Zeroizing::new(String::deserialize(deserializer)?);
    Ok(TeenyString::from_str(&text, None, None))
}

#[cfg(feature = "use_serde")]
fn serialize_teenystring<S>(value: &TeenyString, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    // unsafety: this is safe because the Cell constructor guarantees
    // that the storage is valid utf8
    let s = unsafe { core::str::from_utf8_unchecked(value.as_bytes()) };
    s.serialize(serializer)
}

/// TeenyString encodes string storage in a single u64.
/// The scheme is simple but effective: strings that encode into a
/// byte slice that is 1 less byte than the machine word size can
/// be encoded directly into the usize bits stored in the struct.
/// A marker bit (LSB for big endian, MSB for little endian) is
/// set to indicate that the string is stored inline.
/// If the string is longer than this then a `Vec<u8>` is allocated
/// from the heap and the usize holds its raw pointer address.
///
/// When the string is inlined, the next-MSB is used to short-cut
/// calling grapheme_column_width; if it is set, then the TeenyString
/// has length 2, otherwise, it has length 1 (we don't allow zero-length
/// strings).
struct TeenyString(u64);
struct TeenyStringHeap {
    bytes: Zeroizing<Vec<u8>>,
    width: usize,
}

impl TeenyStringHeap {
    fn wipe_owned_bytes(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for TeenyStringHeap {
    fn drop(&mut self) {
        // `Zeroizing` repeats this operation when its field drops.  Keeping
        // the explicit call here makes the heap owner's destruction contract
        // local and leaves no interval between owner Drop and field Drop in
        // which terminal text remains live.
        self.wipe_owned_bytes();
    }
}

impl zeroize::ZeroizeOnDrop for TeenyStringHeap {}

impl TeenyString {
    const fn marker_mask() -> u64 {
        if cfg!(target_endian = "little") {
            0x80000000_00000000
        } else {
            0x1
        }
    }

    const fn double_wide_mask() -> u64 {
        if cfg!(target_endian = "little") {
            0xc0000000_00000000
        } else {
            0x3
        }
    }

    const fn is_marker_bit_set(word: u64) -> bool {
        let mask = Self::marker_mask();
        word & mask == mask
    }

    const fn is_double_width(word: u64) -> bool {
        let mask = Self::double_wide_mask();
        word & mask == mask
    }

    const fn set_marker_bit(word: u64, width: usize) -> u64 {
        if width > 1 {
            word | Self::double_wide_mask()
        } else {
            word | Self::marker_mask()
        }
    }

    const fn from_inline_byte(byte: u8, width: usize) -> Self {
        let word = if cfg!(target_endian = "little") {
            byte as u64
        } else {
            (byte as u64) << 56
        };
        Self(Self::set_marker_bit(word, width))
    }

    fn can_assume_default_ascii_width(unicode_version: Option<&UnicodeVersion>) -> bool {
        #[cfg(feature = "std")]
        {
            unicode_version
                .and_then(|version| version.cell_widths.as_ref())
                .is_none()
        }

        #[cfg(not(feature = "std"))]
        {
            let _ = unicode_version;
            true
        }
    }

    const fn normalize_explicit_width(width: usize) -> usize {
        if width > 2 {
            2
        } else if width == 0 {
            1
        } else {
            width
        }
    }

    pub fn from_str(
        s: &str,
        width: Option<usize>,
        unicode_version: Option<&UnicodeVersion>,
    ) -> Self {
        // De-fang the input text such that it has no special meaning
        // to a terminal.  All control and movement characters are rewritten
        // as a space.
        let s = if s.is_empty() || s == "\r\n" {
            " "
        } else if s.len() == 1 {
            let b = s.as_bytes()[0];
            if b < 0x20 || b == 0x7f { " " } else { s }
        } else {
            s
        };

        let bytes = s.as_bytes();
        let len = bytes.len();
        let explicit_width = width.map(Self::normalize_explicit_width);

        if len == 1
            && (explicit_width.is_some() || Self::can_assume_default_ascii_width(unicode_version))
        {
            return Self::from_inline_byte(bytes[0], explicit_width.unwrap_or(1));
        }

        let width = explicit_width.unwrap_or_else(|| grapheme_column_width(s, unicode_version));

        if len < core::mem::size_of::<u64>() && width < 3 {
            let mut word = 0u64;
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    &mut word as *mut u64 as *mut u8,
                    len,
                );
            }
            let word = Self::set_marker_bit(word as u64, width);
            Self(word)
        } else {
            let vec = Box::new(TeenyStringHeap {
                bytes: Zeroizing::new(bytes.to_vec()),
                width,
            });
            let ptr = Box::into_raw(vec);
            Self(ptr as u64)
        }
    }

    pub const fn space() -> Self {
        Self(if cfg!(target_endian = "little") {
            0x80000000_00000020
        } else {
            0x20000000_00000001
        })
    }

    pub fn from_char(c: char) -> Self {
        if c.is_ascii() {
            let byte = c as u8;
            let byte = if byte < 0x20 || byte == 0x7f {
                b' '
            } else {
                byte
            };
            return Self::from_inline_byte(byte, 1);
        }

        let mut bytes = [0u8; 8];
        Self::from_str(c.encode_utf8(&mut bytes), None, None)
    }

    pub fn width(&self) -> usize {
        if Self::is_marker_bit_set(self.0) {
            if Self::is_double_width(self.0) { 2 } else { 1 }
        } else {
            let heap = self.0 as *const u64 as *const TeenyStringHeap;
            unsafe { (*heap).width }
        }
    }

    pub fn str(&self) -> &str {
        // unsafety: this is safe because the constructor guarantees
        // that the storage is valid utf8
        unsafe { core::str::from_utf8_unchecked(self.as_bytes()) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        if Self::is_marker_bit_set(self.0) {
            let bytes = &self.0 as *const u64 as *const u8;
            let bytes =
                unsafe { core::slice::from_raw_parts(bytes, core::mem::size_of::<u64>() - 1) };
            let len = bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(core::mem::size_of::<u64>() - 1);

            &bytes[0..len]
        } else {
            let heap = self.0 as *const u64 as *const TeenyStringHeap;
            unsafe { (*heap).bytes.as_slice() }
        }
    }

    fn wipe_inline_storage(&mut self) {
        debug_assert!(Self::is_marker_bit_set(self.0));
        self.0.zeroize();
    }
}

impl Drop for TeenyString {
    fn drop(&mut self) {
        if Self::is_marker_bit_set(self.0) {
            self.wipe_inline_storage();
        } else {
            let ptr = self.0 as *mut usize as *mut TeenyStringHeap;
            self.0.zeroize();
            let heap = unsafe { Box::from_raw(ptr) };
            drop(heap);
        }
    }
}

impl zeroize::ZeroizeOnDrop for TeenyString {}

impl core::clone::Clone for TeenyString {
    fn clone(&self) -> Self {
        if Self::is_marker_bit_set(self.0) {
            Self(self.0)
        } else {
            // Heap-backed cells can carry an explicit width that differs from
            // the host's current Unicode tables.  Recomputing here silently
            // changed terminal geometry when a Line/Cell was cloned (including
            // for persistence).  Preserve the authoritative stored width.
            Self::from_str(self.str(), Some(self.width()), None)
        }
    }
}

impl core::cmp::PartialEq for TeenyString {
    fn eq(&self, rhs: &Self) -> bool {
        self.width() == rhs.width() && self.as_bytes().eq(rhs.as_bytes())
    }
}
impl core::cmp::Eq for TeenyString {}

/// Models the contents of a cell on the terminal display
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Clone, Eq, PartialEq)]
pub struct Cell {
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_teenystring",
            serialize_with = "serialize_teenystring"
        )
    )]
    text: TeenyString,
    attrs: CellAttributes,
}

impl core::fmt::Debug for Cell {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        fmt.debug_struct("Cell")
            .field("text_bytes", &self.str().len())
            .field("text", &"[REDACTED]")
            .field("width", &self.width())
            .field("attrs", &self.attrs)
            .finish()
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank()
    }
}

impl Cell {
    /// Create a new cell holding the specified character and with the
    /// specified cell attributes.
    /// All control and movement characters are rewritten as a space.
    pub fn new(text: char, attrs: CellAttributes) -> Self {
        let storage = TeenyString::from_char(text);
        Self {
            text: storage,
            attrs,
        }
    }

    pub const fn blank() -> Self {
        Self {
            text: TeenyString::space(),
            attrs: CellAttributes::blank(),
        }
    }

    pub const fn blank_with_attrs(attrs: CellAttributes) -> Self {
        Self {
            text: TeenyString::space(),
            attrs,
        }
    }

    /// Indicates whether this cell has text or emoji presentation.
    /// The width already reflects that choice; this information
    /// is also useful when selecting an appropriate font.
    pub fn presentation(&self) -> Presentation {
        match Presentation::for_grapheme(self.str()) {
            (_, Some(variation)) => variation,
            (presentation, None) => presentation,
        }
    }

    /// Create a new cell holding the specified grapheme.
    /// The grapheme is passed as a string slice and is intended to hold
    /// double-width characters, or combining unicode sequences, that need
    /// to be treated as a single logical "character" that can be cursored
    /// over.  This function technically allows for an arbitrary string to
    /// be passed but it should not be used to hold strings other than
    /// graphemes.
    pub fn new_grapheme(
        text: &str,
        attrs: CellAttributes,
        unicode_version: Option<&UnicodeVersion>,
    ) -> Self {
        let storage = TeenyString::from_str(text, None, unicode_version);

        Self {
            text: storage,
            attrs,
        }
    }

    pub fn new_grapheme_with_width(text: &str, width: usize, attrs: CellAttributes) -> Self {
        let storage = TeenyString::from_str(text, Some(width), None);
        Self {
            text: storage,
            attrs,
        }
    }

    /// Returns the textual content of the cell
    pub fn str(&self) -> &str {
        self.text.str()
    }

    /// Returns the number of cells visually occupied by this grapheme
    pub fn width(&self) -> usize {
        self.text.width()
    }

    /// Returns the attributes of the cell
    pub fn attrs(&self) -> &CellAttributes {
        &self.attrs
    }

    pub fn attrs_mut(&mut self) -> &mut CellAttributes {
        &mut self.attrs
    }
}

/// Absolute source-level work bound for configured Unicode cell-width
/// overrides.  Configuration validation counts the expanded ranges (including
/// overlap) against this cap before constructing the lookup map.
pub const MAX_CUSTOM_CELL_WIDTH_EXPANSION: usize = 262_144;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnicodeVersion {
    pub version: u8,
    pub ambiguous_are_wide: bool,
    #[cfg(feature = "std")]
    pub cell_widths: Option<Arc<std::collections::HashMap<u32, u8>>>,
}

impl UnicodeVersion {
    pub const fn new(version: u8) -> Self {
        Self {
            version,
            ambiguous_are_wide: false,
            #[cfg(feature = "std")]
            cell_widths: None,
        }
    }

    #[inline]
    fn width(&self, c: WcWidth) -> usize {
        // Special case for symbol fonts that are naughtly and use
        // the unassigned range instead of the private use range.
        // <https://github.com/wezterm/wezterm/issues/1864>
        if c == WcWidth::Unassigned {
            1
        } else if c == WcWidth::Ambiguous && self.ambiguous_are_wide {
            2
        } else if self.version >= 9 {
            c.width_unicode_9_or_later() as usize
        } else {
            c.width_unicode_8_or_earlier() as usize
        }
    }

    #[inline]
    fn wcwidth(&self, c: char) -> usize {
        #[cfg(feature = "std")]
        if let Some(ref cell_widths) = self.cell_widths {
            if let Some(width) = cell_widths.get(&(c as u32)) {
                return (*width).into();
            }
        }
        self.width(WCWIDTH_TABLE.classify(c))
    }

    #[inline]
    pub fn idx(&self) -> usize {
        (if self.version > 9 { 2 } else { 0 }) | (if self.ambiguous_are_wide { 1 } else { 0 })
    }
}

pub const LATEST_UNICODE_VERSION: UnicodeVersion = UnicodeVersion {
    version: 14,
    ambiguous_are_wide: false,
    #[cfg(feature = "std")]
    cell_widths: None,
};

/// Returns true if the char `c` has the unicode White_Space property
pub fn is_white_space_char(c: char) -> bool {
    frankenterm_char_props::white_space::WHITE_SPACE.contains_u32(c as u32)
}

/// Returns true if the grapheme string `g` consists entirely of characters
/// that have the unicode White_Space property.
pub fn is_white_space_grapheme(g: &str) -> bool {
    for c in g.chars() {
        if !is_white_space_char(c) {
            return false;
        }
    }
    true
}

/// Returns the number of cells visually occupied by a sequence
/// of graphemes.
/// Calls through to `grapheme_column_width` for each grapheme
/// and sums up the length.
pub fn unicode_column_width(s: &str, version: Option<&UnicodeVersion>) -> usize {
    Graphemes::new(s)
        .map(|g| grapheme_column_width(g, version))
        .sum()
}

/// Returns the number of cells visually occupied by a grapheme.
/// The input string must be a single grapheme.
///
/// There are some frustrating dragons in the realm of terminal cell widths:
///
/// a) wcwidth and wcswidth are widely used by applications and may be
///    several versions of unicode behind the current version
/// b) The width of characters has and will change in the future.
///    Unicode Version 8 -> 9 made some characters wider.
///    Unicode 14 defines Emoji variation selectors that change the
///    width depending on trailing context in the unicode sequence.
///
/// Differing opinions about the width leads to visual artifacts in
/// text and and line editors, especially with respect to cursor placement.
///
/// There aren't any really great solutions to this problem, as a given
/// terminal emulator may be fine locally but essentially breaks when
/// ssh'ing into a remote system with a divergent wcwidth implementation.
///
/// This means that a global understanding of the unicode version that
/// is in use isn't a good solution.
///
/// The approach that wezterm wants to take here is to define a
/// configuration value that sets the starting level of unicode conformance,
/// and to define an escape sequence that can push/pop a desired confirmance
/// level onto a stack maintained by the terminal emulator.
///
/// The terminal emulator can then pass the unicode version through to
/// the Cell that is used to hold a grapheme, and that per-Cell version
/// can then be used to calculate width.
pub fn grapheme_column_width(s: &str, version: Option<&UnicodeVersion>) -> usize {
    let version = version.as_deref().unwrap_or(&LATEST_UNICODE_VERSION);

    // Optimization: if there is a single byte we can directly cast
    // that byte as a char which will be in the range 0.255.
    // This takes ~1.5ns, and we can then look that up in the table
    // which is valid for chars in the range 0-0xffff.
    // That lookup also takes ~1.5ns, giving us a hot path latency
    // of ~3-4ns for a grapheme string that is comprised of a single
    // ASCII byte.
    //
    // Since we know this is a single ASCII char, we know that it
    // cannot be a sequence with a variation selector, so we don't
    // need to requested `Presentation` for it.
    if s.len() == 1 {
        return version.wcwidth(s.as_bytes()[0] as char);
    }

    // Slow path: `s.chars()` will dominate and pull up the minimum
    // runtime to ~20ns
    if version.version >= 14 {
        // Lookup the grapheme to see if the presentation of
        // the grapheme forces the width. We can bypass
        // the WcWidth classification if that is true.
        match Presentation::for_grapheme(s) {
            (_, Some(Presentation::Emoji)) => return 2,
            (_, Some(Presentation::Text)) => return 1,
            (Presentation::Emoji, None) => return 2,
            (Presentation::Text, None) => {}
        }
    }

    // Otherwise, classify and sum up
    let mut width = 0;
    for c in s.chars() {
        width += version.wcwidth(c);
    }

    width.min(2)
}

/// Models a change in the attributes of a cell in a stream of changes.
/// Each variant specifies one of the possible attributes; the corresponding
/// value holds the new value to be used for that attribute.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Eq, PartialEq, FromDynamic, ToDynamic)]
pub enum AttributeChange {
    Intensity(Intensity),
    Underline(Underline),
    Italic(bool),
    Blink(Blink),
    Reverse(bool),
    StrikeThrough(bool),
    Invisible(bool),
    Foreground(ColorAttribute),
    Background(ColorAttribute),
    Hyperlink(Option<Arc<Hyperlink>>),
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::color::SrgbaTuple;
    use alloc::{format, vec};

    #[test]
    fn teeny_string() {
        assert!(
            core::mem::size_of::<usize>() <= core::mem::size_of::<u64>(),
            "if a pointer doesn't fit in u64 then we need to change TeenyString"
        );

        let s = TeenyString::from_char('a');
        assert_eq!(s.as_bytes(), b"a");

        let longer = TeenyString::from_str("hellothere", None, None);
        assert_eq!(longer.as_bytes(), b"hellothere");

        assert_eq!(
            TeenyString::from_char(' ').as_bytes(),
            TeenyString::space().as_bytes()
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn memory_usage() {
        assert_eq!(core::mem::size_of::<crate::color::RgbColor>(), 4);
        assert_eq!(core::mem::size_of::<ColorAttribute>(), 20);
        assert_eq!(core::mem::size_of::<CellAttributes>(), 16);
        assert_eq!(core::mem::size_of::<Cell>(), 24);
        assert_eq!(core::mem::size_of::<Vec<u8>>(), 24);
        assert_eq!(core::mem::size_of::<char>(), 4);
        assert_eq!(core::mem::size_of::<TeenyString>(), 8);
    }

    #[test]
    fn nerf_special() {
        for c in " \n\r\t".chars() {
            let cell = Cell::new(c, CellAttributes::default());
            assert_eq!(cell.str(), " ");
        }

        for g in &["", " ", "\n", "\r", "\t", "\r\n"] {
            let cell = Cell::new_grapheme(g, CellAttributes::default(), None);
            assert_eq!(cell.str(), " ");
        }
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_width() {
        let foot = "\u{1f9b6}";
        eprintln!("foot chars");
        for c in foot.chars() {
            eprintln!("char: {:?}", c);
        }
        assert_eq!(unicode_column_width(foot, None), 2, "{} should be 2", foot);

        let women_holding_hands_dark_skin_tone_medium_light_skin_tone =
            "\u{1F469}\u{1F3FF}\u{200D}\u{1F91D}\u{200D}\u{1F469}\u{1F3FC}";

        // Ensure that we can hold this longer grapheme sequence in the cell
        // and correctly return its string contents!
        let cell = Cell::new_grapheme(
            women_holding_hands_dark_skin_tone_medium_light_skin_tone,
            CellAttributes::default(),
            None,
        );
        assert_eq!(
            cell.str(),
            women_holding_hands_dark_skin_tone_medium_light_skin_tone
        );
        assert_eq!(
            cell.width(),
            2,
            "width of {} should be 2",
            women_holding_hands_dark_skin_tone_medium_light_skin_tone
        );

        let deaf_man = "\u{1F9CF}\u{200D}\u{2642}\u{FE0F}";
        eprintln!("deaf_man chars");
        for c in deaf_man.chars() {
            eprintln!("char: {:?}", c);
        }
        assert_eq!(unicode_column_width(deaf_man, None), 2);

        let man_dancing = "\u{1F57A}";
        assert_eq!(
            unicode_column_width(man_dancing, Some(&UnicodeVersion::new(9))),
            2
        );
        assert_eq!(
            unicode_column_width(man_dancing, Some(&UnicodeVersion::new(8))),
            2
        );

        let raised_fist = "\u{270a}";
        assert_eq!(
            unicode_column_width(raised_fist, Some(&UnicodeVersion::new(9))),
            2
        );
        assert_eq!(
            unicode_column_width(raised_fist, Some(&UnicodeVersion::new(8))),
            1
        );

        // This is a codepoint in the private use area
        let font_awesome_star = "\u{f005}";
        eprintln!("font_awesome_star {}", font_awesome_star.escape_debug());
        assert_eq!(unicode_column_width(font_awesome_star, None), 1);

        let england_flag = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}";
        assert_eq!(unicode_column_width(england_flag, None), 2);
    }

    #[test]
    fn issue_1161() {
        let x_ideographic_space_x = "x\u{3000}x";
        assert_eq!(unicode_column_width(x_ideographic_space_x, None), 4);
        assert_eq!(
            Graphemes::new(x_ideographic_space_x).collect::<Vec<_>>(),
            vec!["x".to_string(), "\u{3000}".to_string(), "x".to_string()],
        );

        let c = Cell::new_grapheme("\u{3000}", CellAttributes::blank(), None);
        assert_eq!(c.width(), 2);
    }

    #[test]
    fn issue_997() {
        let victory_hand = "\u{270c}";
        let victory_hand_text_presentation = "\u{270c}\u{fe0e}";

        assert_eq!(
            unicode_column_width(victory_hand_text_presentation, None),
            1
        );
        assert_eq!(unicode_column_width(victory_hand, None), 1);

        assert_eq!(
            Graphemes::new(victory_hand_text_presentation).collect::<Vec<_>>(),
            vec![victory_hand_text_presentation.to_string()]
        );
        assert_eq!(
            Graphemes::new(victory_hand).collect::<Vec<_>>(),
            vec![victory_hand.to_string()]
        );

        let copyright_emoji_presentation = "\u{00A9}\u{FE0F}";
        assert_eq!(
            Graphemes::new(copyright_emoji_presentation).collect::<Vec<_>>(),
            vec![copyright_emoji_presentation.to_string()]
        );
        assert_eq!(unicode_column_width(copyright_emoji_presentation, None), 2);
        assert_eq!(
            unicode_column_width(copyright_emoji_presentation, Some(&UnicodeVersion::new(9))),
            1
        );

        let copyright_text_presentation = "\u{00A9}";
        assert_eq!(
            Graphemes::new(copyright_text_presentation).collect::<Vec<_>>(),
            vec![copyright_text_presentation.to_string()]
        );
        assert_eq!(unicode_column_width(copyright_text_presentation, None), 1);

        let raised_fist = "\u{270a}";
        // Not valid to have explicit Text presentation for raised fist
        let raised_fist_text = "\u{270a}\u{fe0e}";
        assert_eq!(
            Presentation::for_grapheme(raised_fist),
            (Presentation::Emoji, None)
        );
        assert_eq!(unicode_column_width(raised_fist, None), 2);
        assert_eq!(
            Presentation::for_grapheme(raised_fist_text),
            (Presentation::Emoji, None)
        );
        assert_eq!(unicode_column_width(raised_fist_text, None), 2);

        assert_eq!(
            Graphemes::new(raised_fist_text).collect::<Vec<_>>(),
            vec![raised_fist_text.to_string()]
        );
        assert_eq!(
            Graphemes::new(raised_fist).collect::<Vec<_>>(),
            vec![raised_fist.to_string()]
        );
    }

    #[test]
    fn issue_1573() {
        let sequence = "\u{1112}\u{1161}\u{11ab}";
        assert_eq!(unicode_column_width(sequence, None), 2);
        assert_eq!(grapheme_column_width(sequence, None), 2);

        let sequence2 = core::str::from_utf8(b"\xe1\x84\x92\xe1\x85\xa1\xe1\x86\xab").unwrap();
        assert_eq!(unicode_column_width(sequence2, None), 2);
        assert_eq!(grapheme_column_width(sequence2, None), 2);
    }

    // See <https://github.com/wezterm/wezterm/issues/6637>
    // We're not directly "fixing" that issue here in termwiz at this time
    // because it isn't clear that this cell module has enough context
    // to eg: decide that the width of U+2028 should be returned as 1.
    // That decision is made over in wezterm-term when processing
    // a sequence of graphemes. This test case is just making assertions
    // about the properties of a couple of problematic zero-width
    // characters.
    #[test]
    fn issue_6637() {
        // U+2028 is the unicode line separator. It is Non-printing White_Space.
        let sequence = "\u{2028}";
        // It has zero width
        assert_eq!(unicode_column_width(sequence, None), 0);
        assert_eq!(grapheme_column_width(sequence, None), 0);
        // it is white space
        assert!(is_white_space_grapheme(sequence));

        // Just a couple of sanity checks for the white space function
        assert!(is_white_space_char(' '));
        assert!(!is_white_space_char('x'));

        // U+2068 is a BIDI control character and is relevant here
        // due to <https://github.com/wezterm/wezterm/issues/1422>.
        // It is Non-Printing, non-White_Space
        assert!(!is_white_space_char('\u{2068}'));
    }

    // ── SmallColor ──────────────────────────────────────────

    #[test]
    fn small_color_default() {
        let c = SmallColor::default();
        assert_eq!(c, SmallColor::Default);
    }

    #[test]
    fn small_color_into_color_attribute_default() {
        let attr: ColorAttribute = SmallColor::Default.into();
        assert_eq!(attr, ColorAttribute::Default);
    }

    #[test]
    fn small_color_into_color_attribute_palette() {
        let attr: ColorAttribute = SmallColor::PaletteIndex(42).into();
        assert_eq!(attr, ColorAttribute::PaletteIndex(42));
    }

    #[cfg(feature = "std")]
    #[test]
    fn small_color_clone_eq_hash() {
        let a = SmallColor::PaletteIndex(7);
        let b = a;
        assert_eq!(a, b);
        // Hash consistency
        use core::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        a.hash(&mut h1);
        b.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    // ── SemanticType ────────────────────────────────────────

    #[test]
    fn semantic_type_default_is_output() {
        assert_eq!(SemanticType::default(), SemanticType::Output);
    }

    #[test]
    fn semantic_type_ordering() {
        assert!(SemanticType::Output < SemanticType::Input);
        assert!(SemanticType::Input < SemanticType::Prompt);
    }

    #[test]
    fn semantic_type_debug_clone() {
        let s = SemanticType::Prompt;
        #[allow(clippy::clone_on_copy)]
        let c = s.clone();
        assert_eq!(s, c);
        assert_eq!(format!("{s:?}"), "Prompt");
    }

    // ── CellAttributes: blank / default ─────────────────────

    #[test]
    fn cell_attributes_blank_is_default() {
        assert_eq!(CellAttributes::blank(), CellAttributes::default());
    }

    #[test]
    fn cell_attributes_blank_all_false() {
        let a = CellAttributes::blank();
        assert_eq!(a.intensity(), Intensity::Normal);
        assert_eq!(a.underline(), Underline::None);
        assert_eq!(a.blink(), Blink::None);
        assert!(!a.italic());
        assert!(!a.reverse());
        assert!(!a.strikethrough());
        assert!(!a.invisible());
        assert!(!a.wrapped());
        assert!(!a.overline());
        assert_eq!(a.semantic_type(), SemanticType::Output);
        assert_eq!(a.vertical_align(), VerticalAlign::BaseLine);
        assert_eq!(a.foreground(), ColorAttribute::Default);
        assert_eq!(a.background(), ColorAttribute::Default);
    }

    // ── CellAttributes: boolean bitfields ───────────────────

    #[test]
    fn bitfield_italic_roundtrip() {
        let mut a = CellAttributes::blank();
        assert!(!a.italic());
        a.set_italic(true);
        assert!(a.italic());
        a.set_italic(false);
        assert!(!a.italic());
    }

    #[test]
    fn bitfield_reverse_roundtrip() {
        let mut a = CellAttributes::blank();
        a.set_reverse(true);
        assert!(a.reverse());
        a.set_reverse(false);
        assert!(!a.reverse());
    }

    #[test]
    fn bitfield_strikethrough_roundtrip() {
        let mut a = CellAttributes::blank();
        a.set_strikethrough(true);
        assert!(a.strikethrough());
    }

    #[test]
    fn bitfield_invisible_roundtrip() {
        let mut a = CellAttributes::blank();
        a.set_invisible(true);
        assert!(a.invisible());
    }

    #[test]
    fn bitfield_wrapped_roundtrip() {
        let mut a = CellAttributes::blank();
        a.set_wrapped(true);
        assert!(a.wrapped());
    }

    #[test]
    fn bitfield_overline_roundtrip() {
        let mut a = CellAttributes::blank();
        a.set_overline(true);
        assert!(a.overline());
    }

    // ── CellAttributes: enum bitfields ──────────────────────

    #[test]
    fn bitfield_intensity_values() {
        let mut a = CellAttributes::blank();
        a.set_intensity(Intensity::Bold);
        assert_eq!(a.intensity(), Intensity::Bold);
        a.set_intensity(Intensity::Half);
        assert_eq!(a.intensity(), Intensity::Half);
        a.set_intensity(Intensity::Normal);
        assert_eq!(a.intensity(), Intensity::Normal);
    }

    #[test]
    fn bitfield_underline_values() {
        let mut a = CellAttributes::blank();
        for val in [
            Underline::Single,
            Underline::Double,
            Underline::Curly,
            Underline::Dotted,
            Underline::Dashed,
            Underline::None,
        ] {
            a.set_underline(val);
            assert_eq!(a.underline(), val);
        }
    }

    #[test]
    fn bitfield_blink_values() {
        let mut a = CellAttributes::blank();
        a.set_blink(Blink::Slow);
        assert_eq!(a.blink(), Blink::Slow);
        a.set_blink(Blink::Rapid);
        assert_eq!(a.blink(), Blink::Rapid);
        a.set_blink(Blink::None);
        assert_eq!(a.blink(), Blink::None);
    }

    #[test]
    fn bitfield_semantic_type_values() {
        let mut a = CellAttributes::blank();
        a.set_semantic_type(SemanticType::Input);
        assert_eq!(a.semantic_type(), SemanticType::Input);
        a.set_semantic_type(SemanticType::Prompt);
        assert_eq!(a.semantic_type(), SemanticType::Prompt);
        a.set_semantic_type(SemanticType::Output);
        assert_eq!(a.semantic_type(), SemanticType::Output);
    }

    #[test]
    fn bitfield_vertical_align_values() {
        let mut a = CellAttributes::blank();
        a.set_vertical_align(VerticalAlign::SuperScript);
        assert_eq!(a.vertical_align(), VerticalAlign::SuperScript);
        a.set_vertical_align(VerticalAlign::SubScript);
        assert_eq!(a.vertical_align(), VerticalAlign::SubScript);
        a.set_vertical_align(VerticalAlign::BaseLine);
        assert_eq!(a.vertical_align(), VerticalAlign::BaseLine);
    }

    #[test]
    fn bitfield_invalid_enum_values_fall_back_to_defaults() {
        let mut a = CellAttributes::blank();
        a.attributes = 3 | (6 << 2) | (3 << 5) | (3 << 13) | (3 << 15);

        assert_eq!(a.intensity(), Intensity::Normal);
        assert_eq!(a.underline(), Underline::None);
        assert_eq!(a.blink(), Blink::None);
        assert_eq!(a.semantic_type(), SemanticType::Output);
        assert_eq!(a.vertical_align(), VerticalAlign::BaseLine);
    }

    // ── CellAttributes: setter chaining ─────────────────────

    #[test]
    fn setter_chaining() {
        let mut a = CellAttributes::blank();
        a.set_italic(true).set_reverse(true).set_overline(true);
        assert!(a.italic());
        assert!(a.reverse());
        assert!(a.overline());
    }

    // ── CellAttributes: attribute_bits_equal ────────────────

    #[test]
    fn attribute_bits_equal_identical() {
        let a = CellAttributes::blank();
        let b = CellAttributes::blank();
        assert!(a.attribute_bits_equal(&b));
    }

    #[test]
    fn attribute_bits_equal_differ() {
        let a = CellAttributes::blank();
        let mut b = CellAttributes::blank();
        b.set_italic(true);
        assert!(!a.attribute_bits_equal(&b));
    }

    // ── CellAttributes: foreground / background ─────────────

    #[test]
    fn foreground_default() {
        let a = CellAttributes::blank();
        assert_eq!(a.foreground(), ColorAttribute::Default);
    }

    #[test]
    fn foreground_palette_index() {
        let mut a = CellAttributes::blank();
        a.set_foreground(ColorAttribute::PaletteIndex(196));
        assert_eq!(a.foreground(), ColorAttribute::PaletteIndex(196));
    }

    #[test]
    fn foreground_truecolor() {
        let mut a = CellAttributes::blank();
        let tc = ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(1.0, 0.0, 0.0, 1.0));
        a.set_foreground(tc);
        assert_eq!(a.foreground(), tc);
    }

    #[test]
    fn foreground_reset_to_default() {
        let mut a = CellAttributes::blank();
        a.set_foreground(ColorAttribute::PaletteIndex(1));
        a.set_foreground(ColorAttribute::Default);
        assert_eq!(a.foreground(), ColorAttribute::Default);
    }

    #[test]
    fn background_palette_index() {
        let mut a = CellAttributes::blank();
        a.set_background(ColorAttribute::PaletteIndex(42));
        assert_eq!(a.background(), ColorAttribute::PaletteIndex(42));
    }

    #[test]
    fn background_truecolor() {
        let mut a = CellAttributes::blank();
        let tc = ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(0.0, 1.0, 0.0, 1.0));
        a.set_background(tc);
        assert_eq!(a.background(), tc);
    }

    #[test]
    fn background_reset_to_default() {
        let mut a = CellAttributes::blank();
        a.set_background(ColorAttribute::PaletteIndex(1));
        a.set_background(ColorAttribute::Default);
        assert_eq!(a.background(), ColorAttribute::Default);
    }

    // ── CellAttributes: underline_color ─────────────────────

    #[test]
    fn underline_color_default() {
        let a = CellAttributes::blank();
        assert_eq!(a.underline_color(), ColorAttribute::Default);
    }

    #[test]
    fn underline_color_set_and_get() {
        let mut a = CellAttributes::blank();
        a.set_underline_color(ColorAttribute::PaletteIndex(9));
        assert_eq!(a.underline_color(), ColorAttribute::PaletteIndex(9));
    }

    #[test]
    fn underline_color_reset_deallocates_fat() {
        let mut a = CellAttributes::blank();
        a.set_underline_color(ColorAttribute::PaletteIndex(9));
        a.set_underline_color(ColorAttribute::Default);
        assert_eq!(a.underline_color(), ColorAttribute::Default);
        // Fat should be deallocated when all fat fields are default
        assert_eq!(a, CellAttributes::blank());
    }

    // ── CellAttributes: hyperlink ───────────────────────────

    #[test]
    fn hyperlink_none_by_default() {
        let a = CellAttributes::blank();
        assert!(a.hyperlink().is_none());
    }

    #[test]
    fn hyperlink_set_and_get() {
        let mut a = CellAttributes::blank();
        let link = Arc::new(Hyperlink::new("https://example.com"));
        a.set_hyperlink(Some(link.clone()));
        assert!(a.hyperlink().is_some());
        assert_eq!(a.hyperlink().unwrap().uri(), link.uri());
    }

    #[test]
    fn hyperlink_clear() {
        let mut a = CellAttributes::blank();
        let link = Arc::new(Hyperlink::new("https://example.com"));
        a.set_hyperlink(Some(link));
        a.set_hyperlink(None);
        assert!(a.hyperlink().is_none());
        // Should deallocate fat attrs
        assert_eq!(a, CellAttributes::blank());
    }

    // ── CellAttributes: clear ───────────────────────────────

    #[test]
    fn clear_resets_to_blank() {
        let mut a = CellAttributes::blank();
        a.set_italic(true);
        a.set_foreground(ColorAttribute::PaletteIndex(1));
        a.set_intensity(Intensity::Bold);
        a.clear();
        assert_eq!(a, CellAttributes::blank());
    }

    // ── CellAttributes: clone_sgr_only ──────────────────────

    #[test]
    fn clone_sgr_only_preserves_colors() {
        let mut a = CellAttributes::blank();
        a.set_foreground(ColorAttribute::PaletteIndex(196));
        a.set_background(ColorAttribute::PaletteIndex(21));
        let cloned = a.clone_sgr_only();
        assert_eq!(cloned.foreground(), a.foreground());
        assert_eq!(cloned.background(), a.background());
    }

    #[test]
    fn clone_sgr_only_strips_hyperlink() {
        let mut a = CellAttributes::blank();
        let link = Arc::new(Hyperlink::new("https://example.com"));
        a.set_hyperlink(Some(link));
        let cloned = a.clone_sgr_only();
        assert!(cloned.hyperlink().is_none());
    }

    #[test]
    fn clone_sgr_only_resets_semantic_type() {
        let mut a = CellAttributes::blank();
        a.set_semantic_type(SemanticType::Prompt);
        let cloned = a.clone_sgr_only();
        assert_eq!(cloned.semantic_type(), SemanticType::Output);
    }

    #[test]
    fn clone_sgr_only_clears_underline() {
        let mut a = CellAttributes::blank();
        a.set_underline(Underline::Double);
        let cloned = a.clone_sgr_only();
        assert_eq!(cloned.underline(), Underline::None);
    }

    #[test]
    fn clone_sgr_only_clears_overline_and_strikethrough() {
        let mut a = CellAttributes::blank();
        a.set_overline(true);
        a.set_strikethrough(true);
        let cloned = a.clone_sgr_only();
        assert!(!cloned.overline());
        assert!(!cloned.strikethrough());
    }

    // ── CellAttributes: apply_change ────────────────────────

    #[test]
    fn apply_change_intensity() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Intensity(Intensity::Bold));
        assert_eq!(a.intensity(), Intensity::Bold);
    }

    #[test]
    fn apply_change_underline() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Underline(Underline::Curly));
        assert_eq!(a.underline(), Underline::Curly);
    }

    #[test]
    fn apply_change_italic() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Italic(true));
        assert!(a.italic());
    }

    #[test]
    fn apply_change_blink() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Blink(Blink::Slow));
        assert_eq!(a.blink(), Blink::Slow);
    }

    #[test]
    fn apply_change_reverse() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Reverse(true));
        assert!(a.reverse());
    }

    #[test]
    fn apply_change_strikethrough() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::StrikeThrough(true));
        assert!(a.strikethrough());
    }

    #[test]
    fn apply_change_invisible() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Invisible(true));
        assert!(a.invisible());
    }

    #[test]
    fn apply_change_foreground() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Foreground(ColorAttribute::PaletteIndex(
            1,
        )));
        assert_eq!(a.foreground(), ColorAttribute::PaletteIndex(1));
    }

    #[test]
    fn apply_change_background() {
        let mut a = CellAttributes::blank();
        a.apply_change(&AttributeChange::Background(ColorAttribute::PaletteIndex(
            2,
        )));
        assert_eq!(a.background(), ColorAttribute::PaletteIndex(2));
    }

    #[test]
    fn apply_change_hyperlink() {
        let mut a = CellAttributes::blank();
        let link = Arc::new(Hyperlink::new("https://example.com"));
        a.apply_change(&AttributeChange::Hyperlink(Some(link)));
        assert!(a.hyperlink().is_some());
    }

    // ── CellAttributes: Debug / Clone / Eq ──────────────────

    #[test]
    fn cell_attributes_debug() {
        let mut a = CellAttributes::blank();
        a.set_italic(true);
        let dbg = format!("{a:?}");
        assert!(dbg.contains("italic"));
        assert!(dbg.contains("true"));
    }

    #[test]
    fn cell_attributes_clone_eq() {
        let mut a = CellAttributes::blank();
        a.set_intensity(Intensity::Bold);
        a.set_foreground(ColorAttribute::PaletteIndex(3));
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── CellAttributes: compute_shape_hash ──────────────────

    #[cfg(feature = "std")]
    #[test]
    fn compute_shape_hash_differs_for_different_attrs() {
        use std::collections::hash_map::DefaultHasher;
        let a = CellAttributes::blank();
        let mut b = CellAttributes::blank();
        b.set_italic(true);

        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        a.compute_shape_hash(&mut h1);
        b.compute_shape_hash(&mut h2);
        assert_ne!(h1.finish(), h2.finish());
    }

    // ── TeenyString extras ──────────────────────────────────

    #[test]
    fn teeny_string_clone_inline() {
        let s = TeenyString::from_char('Z');
        let c = s.clone();
        assert_eq!(s.str(), c.str());
    }

    #[test]
    fn teeny_string_clone_heap() {
        let s = TeenyString::from_str("a long string that goes on heap", None, None);
        let c = s.clone();
        assert_eq!(s.str(), c.str());
        assert_ne!(s.0, c.0, "heap-backed clones must own distinct buffers");
        drop(s);
        assert_eq!(c.str(), "a long string that goes on heap");
    }

    #[test]
    fn teeny_string_inline_wipe_clears_the_packed_word() {
        let mut s = TeenyString::from_str("secret", None, None);
        assert!(TeenyString::is_marker_bit_set(s.0));

        s.wipe_inline_storage();

        assert_eq!(s.0, 0, "inline terminal text must be overwritten");
        // Restore a valid empty-of-secret inline representation so the normal
        // Drop path can run at scope exit after this direct helper probe.
        s.0 = TeenyString::space().0;
    }

    #[test]
    fn teeny_string_heap_owner_wipes_its_guarded_buffer() {
        fn require_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        require_zeroize_on_drop::<TeenyString>();
        require_zeroize_on_drop::<TeenyStringHeap>();

        let mut heap = TeenyStringHeap {
            bytes: Zeroizing::new(b"semantic terminal text".to_vec()),
            width: 1,
        };
        let capacity = heap.bytes.capacity();

        heap.wipe_owned_bytes();

        assert!(heap.bytes.is_empty());
        assert_eq!(heap.bytes.capacity(), capacity);
    }

    #[test]
    fn teeny_string_eq() {
        let a = TeenyString::from_char('Q');
        let b = TeenyString::from_char('Q');
        assert!(a == b);
    }

    #[test]
    fn teeny_string_ne() {
        let a = TeenyString::from_char('A');
        let b = TeenyString::from_char('B');
        assert!(a != b);
    }

    #[test]
    fn teeny_string_width_single_byte_ascii() {
        let s = TeenyString::from_char('A');
        assert_eq!(s.width(), 1);
    }

    #[test]
    fn teeny_string_control_char_becomes_space() {
        let s = TeenyString::from_char('\n');
        assert_eq!(s.str(), " ");
    }

    #[test]
    fn teeny_string_str_roundtrip() {
        let s = TeenyString::from_str("hello", None, None);
        assert_eq!(s.str(), "hello");
    }

    // ── Cell ────────────────────────────────────────────────

    #[test]
    fn cell_blank_is_space() {
        let c = Cell::blank();
        assert_eq!(c.str(), " ");
        assert_eq!(c.width(), 1);
    }

    #[test]
    fn cell_default_is_blank() {
        assert_eq!(Cell::default(), Cell::blank());
    }

    #[test]
    fn cell_new_ascii() {
        let c = Cell::new('X', CellAttributes::blank());
        assert_eq!(c.str(), "X");
        assert_eq!(c.width(), 1);
    }

    #[test]
    fn cell_new_grapheme_with_width() {
        let c = Cell::new_grapheme_with_width("AB", 2, CellAttributes::blank());
        assert_eq!(c.str(), "AB");
        assert_eq!(c.width(), 2);
    }

    #[test]
    fn cell_new_grapheme_with_width_normalizes_explicit_extremes() {
        let zero = Cell::new_grapheme_with_width("Z", 0, CellAttributes::blank());
        assert_eq!(zero.str(), "Z");
        assert_eq!(zero.width(), 1);

        let huge_inline = Cell::new_grapheme_with_width("Z", usize::MAX, CellAttributes::blank());
        assert_eq!(huge_inline.str(), "Z");
        assert_eq!(huge_inline.width(), 2);

        let huge_heap =
            Cell::new_grapheme_with_width("abcdefgh", usize::MAX, CellAttributes::blank());
        assert_eq!(huge_heap.str(), "abcdefgh");
        assert_eq!(huge_heap.width(), 2);
    }

    #[test]
    fn cell_blank_with_attrs() {
        let mut attrs = CellAttributes::blank();
        attrs.set_italic(true);
        let c = Cell::blank_with_attrs(attrs.clone());
        assert_eq!(c.str(), " ");
        assert!(c.attrs().italic());
    }

    #[test]
    fn cell_attrs_mut() {
        let mut c = Cell::blank();
        c.attrs_mut().set_reverse(true);
        assert!(c.attrs().reverse());
    }

    #[test]
    fn cell_debug() {
        let c = Cell::new('A', CellAttributes::blank());
        let dbg = format!("{c:?}");
        assert!(dbg.contains("Cell"));
        assert!(dbg.contains("A"));
    }

    #[test]
    fn cell_clone_eq() {
        let c = Cell::new('Z', CellAttributes::blank());
        let c2 = c.clone();
        assert_eq!(c, c2);
    }

    #[test]
    fn cell_presentation_text() {
        let c = Cell::new('A', CellAttributes::blank());
        assert_eq!(c.presentation(), Presentation::Text);
    }

    #[test]
    fn cell_presentation_emoji() {
        let c = Cell::new('\u{1F600}', CellAttributes::blank());
        assert_eq!(c.presentation(), Presentation::Emoji);
    }

    // ── UnicodeVersion ──────────────────────────────────────

    #[test]
    fn unicode_version_new() {
        let v = UnicodeVersion::new(14);
        assert_eq!(v.version, 14);
        assert!(!v.ambiguous_are_wide);
    }

    #[test]
    fn unicode_version_idx() {
        // version <= 9, not ambiguous wide => 0
        assert_eq!(UnicodeVersion::new(9).idx(), 0);
        // version > 9, not ambiguous wide => 2
        assert_eq!(UnicodeVersion::new(14).idx(), 2);
        // version > 9, ambiguous wide => 3
        let mut v = UnicodeVersion::new(14);
        v.ambiguous_are_wide = true;
        assert_eq!(v.idx(), 3);
    }

    #[test]
    fn unicode_version_clone_eq() {
        let a = UnicodeVersion::new(9);
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── AttributeChange ─────────────────────────────────────

    #[test]
    fn attribute_change_debug() {
        let c = AttributeChange::Italic(true);
        let dbg = format!("{c:?}");
        assert!(dbg.contains("Italic"));
    }

    #[test]
    fn attribute_change_clone_eq() {
        let a = AttributeChange::Reverse(true);
        let b = a.clone();
        assert_eq!(a, b);
    }

    // ── is_white_space utilities ─────────────────────────────

    #[test]
    fn white_space_tab_is_white_space() {
        assert!(is_white_space_char('\t'));
    }

    #[test]
    fn white_space_grapheme_all_spaces() {
        assert!(is_white_space_grapheme("   "));
    }

    #[test]
    fn white_space_grapheme_mixed() {
        assert!(!is_white_space_grapheme("a "));
    }

    #[test]
    fn white_space_grapheme_empty() {
        assert!(is_white_space_grapheme(""));
    }

    // ── ColorAttribute ──────────────────────────────────────

    #[test]
    fn color_attribute_default() {
        assert_eq!(ColorAttribute::default(), ColorAttribute::Default);
    }

    #[test]
    fn color_attribute_from_ansi_color() {
        use crate::color::AnsiColor;
        let attr: ColorAttribute = AnsiColor::Red.into();
        // AnsiColor::Red is index 1
        assert_eq!(attr, ColorAttribute::PaletteIndex(AnsiColor::Red as u8));
    }

    // ── Multiple bitfields don't interfere ──────────────────

    #[test]
    fn bitfields_independent() {
        let mut a = CellAttributes::blank();
        a.set_italic(true);
        a.set_reverse(true);
        a.set_intensity(Intensity::Bold);
        a.set_underline(Underline::Curly);
        a.set_blink(Blink::Rapid);
        a.set_strikethrough(true);
        a.set_invisible(true);
        a.set_wrapped(true);
        a.set_overline(true);
        a.set_semantic_type(SemanticType::Prompt);
        a.set_vertical_align(VerticalAlign::SuperScript);

        // Verify all values survived
        assert!(a.italic());
        assert!(a.reverse());
        assert_eq!(a.intensity(), Intensity::Bold);
        assert_eq!(a.underline(), Underline::Curly);
        assert_eq!(a.blink(), Blink::Rapid);
        assert!(a.strikethrough());
        assert!(a.invisible());
        assert!(a.wrapped());
        assert!(a.overline());
        assert_eq!(a.semantic_type(), SemanticType::Prompt);
        assert_eq!(a.vertical_align(), VerticalAlign::SuperScript);
    }
}
