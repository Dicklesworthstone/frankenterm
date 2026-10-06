use crate::line::CellRef;
use core::num::NonZeroU8;
use finl_unicode::grapheme_clusters::Graphemes;
use fixedbitset::FixedBitSet;
use frankenterm_cell::{Cell, CellAttributes};
#[cfg(feature = "use_serde")]
use serde::de::Error as _;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};

extern crate alloc;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use zeroize::{Zeroize, Zeroizing};

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
struct Cluster {
    cell_width: u16,
    attrs: CellAttributes,
}

/// Whether `c` belongs to a code point range whose Grapheme_Cluster_Break is
/// Other or Extended_Pictographic. Such a char neither extends the grapheme
/// before it nor is extended by the one after it (GB6-GB9c, GB11-GB13 all
/// need a Control, Extend, ZWJ, SpacingMark, Prepend, Regional_Indicator,
/// Hangul jamo or InCB consonant on one side). Covered: printable ASCII,
/// Latin-1 (minus U+00AD) through Latin Extended, IPA and spacing modifiers,
/// Greek, Cyrillic, general punctuation (minus the format controls), arrows
/// through dingbats, braille, CJK symbols (minus U+302A-U+302F), kana (minus
/// U+3099-U+309A), CJK ideographs, Hangul syllables, fullwidth forms (minus
/// U+FF9E-U+FF9F) and the emoji blocks minus the skin-tone modifiers
/// U+1F3FB-U+1F3FF. Anything else is treated as able to cluster.
pub(crate) fn is_cluster_inert(c: char) -> bool {
    matches!(
        u32::from(c),
        0x20..=0x7E
            | 0xA0..=0xAC
            | 0xAE..=0x2FF
            | 0x370..=0x482
            | 0x48A..=0x52F
            | 0x2010..=0x2027
            | 0x2030..=0x205E
            | 0x2190..=0x27BF
            | 0x2800..=0x28FF
            | 0x3000..=0x3029
            | 0x3030..=0x303F
            | 0x3041..=0x3096
            | 0x309B..=0x30FF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7A3
            | 0xFF01..=0xFF9D
            | 0x1F300..=0x1F3FA
            | 0x1F400..=0x1F64F
            | 0x1F680..=0x1F6FF
            | 0x1F900..=0x1F9FF
            | 0x1FA70..=0x1FAFF
    )
}

/// Whether a grapheme boundary is guaranteed between a cell whose text ends
/// in `prev` (`None` for the start of the line) and one starting with `next`.
fn breaks_between(prev: Option<char>, next: char) -> bool {
    prev.is_none_or(|prev| is_cluster_inert(prev) && is_cluster_inert(next))
}

/// The rule clustered storage appends by: whether a cell starting with
/// `next` may follow one ending in `prev` (`None` at the start of the line)
/// without the two clustering into one grapheme. The PageGrid engine
/// mirrors which rows stay clustered with it (ft-yccm0.3.3.4).
#[doc(hidden)]
pub fn clustered_append_breaks(prev: Option<char>, next: char) -> bool {
    breaks_between(prev, next)
}

/// Stores line data as a contiguous string and a series of
/// clusters of attribute data describing attributed ranges
/// within the line
#[cfg_attr(feature = "use_serde", derive(Serialize))]
#[derive(PartialEq)]
pub(crate) struct ClusteredLine {
    pub text: String,
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_bitset",
            serialize_with = "serialize_bitset"
        )
    )]
    is_double_wide: Option<Box<FixedBitSet>>,
    clusters: Vec<Cluster>,
    /// Length, measured in cells
    len: u32,
    last_cell_width: Option<NonZeroU8>,
}

impl core::fmt::Debug for ClusteredLine {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ClusteredLine")
            .field("text_bytes", &self.text.len())
            .field("cell_len", &self.len)
            .field("cluster_count", &self.clusters.len())
            .field("text", &"[REDACTED]")
            .finish()
    }
}

fn guarded_reserve_text(target: &mut String, additional: usize) {
    let required = target
        .len()
        .checked_add(additional)
        .expect("clustered line text length overflowed usize");
    if required > target.capacity() {
        let grown_capacity = target.capacity().saturating_mul(2).max(required);
        let mut replacement = Zeroizing::new(String::with_capacity(grown_capacity));
        replacement.push_str(target);
        target.zeroize();
        core::mem::swap(target, &mut *replacement);
    }
}

fn guarded_push_str(target: &mut String, fragment: &str) {
    guarded_reserve_text(target, fragment.len());
    target.push_str(fragment);
}

/// Guards a successfully decoded text field until every later line field has
/// also passed deserialization.
#[cfg(feature = "use_serde")]
struct GuardedLineText(Zeroizing<String>);

#[cfg(feature = "use_serde")]
impl GuardedLineText {
    fn take(&mut self) -> String {
        core::mem::take(&mut *self.0)
    }
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for GuardedLineText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|text| Self(Zeroizing::new(text)))
    }
}

#[cfg(feature = "use_serde")]
impl Drop for GuardedLineText {
    fn drop(&mut self) {
        self.0.zeroize();

        #[cfg(test)]
        GUARDED_LINE_TEXT_WIPE_INVOCATIONS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(all(test, feature = "use_serde"))]
static GUARDED_LINE_TEXT_WIPE_INVOCATIONS: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "use_serde")]
const MAX_DESERIALIZED_WIDE_CELL_BITS: usize = 16 * 1024 * 1024 * 8;

#[cfg(feature = "use_serde")]
fn deserialize_bitset<'de, D>(deserializer: D) -> Result<Option<Box<FixedBitSet>>, D::Error>
where
    D: Deserializer<'de>,
{
    let wide_indices = <Vec<usize>>::deserialize(deserializer)?;
    if wide_indices.is_empty() {
        Ok(None)
    } else {
        let max_idx = wide_indices.iter().copied().max().unwrap_or(1);
        let bit_capacity = max_idx.checked_add(1).ok_or_else(|| {
            D::Error::custom("clustered line wide-cell bitset length overflowed usize")
        })?;
        if bit_capacity > MAX_DESERIALIZED_WIDE_CELL_BITS {
            return Err(D::Error::custom(format!(
                "clustered line wide-cell bitset length {bit_capacity} exceeds maximum {MAX_DESERIALIZED_WIDE_CELL_BITS}"
            )));
        }
        let mut bitset = FixedBitSet::with_capacity(bit_capacity);
        for idx in wide_indices {
            bitset.set(idx, true);
        }
        Ok(Some(Box::new(bitset)))
    }
}

#[cfg(feature = "use_serde")]
#[derive(Deserialize)]
#[serde(rename = "ClusteredLine")]
struct GuardedClusteredLine {
    text: GuardedLineText,
    #[serde(deserialize_with = "deserialize_bitset")]
    is_double_wide: Option<Box<FixedBitSet>>,
    clusters: Vec<Cluster>,
    len: u32,
    last_cell_width: Option<NonZeroU8>,
}

#[cfg(feature = "use_serde")]
impl<'de> Deserialize<'de> for ClusteredLine {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let GuardedClusteredLine {
            mut text,
            is_double_wide,
            clusters,
            len,
            last_cell_width,
        } = GuardedClusteredLine::deserialize(deserializer)?;

        // No fallible work remains after the text leaves its construction
        // guard; the returned ClusteredLine becomes the Drop-hardened owner.
        Ok(Self {
            text: text.take(),
            is_double_wide,
            clusters,
            len,
            last_cell_width,
        })
    }
}

/// Serialize the bitset as a vector of the indices of just the 1 bits;
/// the thesis is that most of the cells on a given line are single width.
/// That may not be strictly true for users that heavily use asian scripts,
/// but we'll start with this and see if we need to improve it.
#[cfg(feature = "use_serde")]
fn serialize_bitset<S>(value: &Option<Box<FixedBitSet>>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut wide_indices: Vec<usize> = vec![];
    if let Some(bits) = value {
        for idx in bits.ones() {
            wide_indices.push(idx);
        }
    }
    wide_indices.serialize(serializer)
}

impl ClusteredLine {
    fn normalize_cell_width(cell_width: usize) -> u16 {
        cell_width.clamp(1, 2) as u16
    }

    pub fn new() -> Self {
        Self {
            text: String::with_capacity(80),
            is_double_wide: None,
            clusters: vec![],
            len: 0,
            last_cell_width: None,
        }
    }

    fn materialize_cell_vec(&self) -> (Vec<Cell>, *const Cell, usize, usize) {
        // A growing Vec bitwise-moves initialized Cells into each replacement
        // allocation.  Inline TeenyString bytes would then remain in the old
        // allocation without running Cell::drop.  Count the exact materialized
        // width first so the plaintext-bearing Cell buffer never reallocates.
        // Iterator widths are normalized to 1 or 2 and its grapheme count is
        // bounded by the backing String allocation.  Saturation is therefore
        // unreachable for a valid allocation, while still keeping arithmetic
        // bounded if a malformed representation reaches this private type.
        let cell_count = self
            .iter()
            .fold(0usize, |count, cell| count.saturating_add(cell.width()));
        let mut cells = Vec::with_capacity(cell_count);
        let reserved_ptr = cells.as_ptr();
        let reserved_capacity = cells.capacity();

        for c in self.iter() {
            cells.push(c.as_cell());
            for _ in 1..c.width() {
                cells.push(Cell::blank_with_attrs(c.attrs().clone()));
            }
        }

        (cells, reserved_ptr, reserved_capacity, cell_count)
    }

    pub fn to_cell_vec(&self) -> Vec<Cell> {
        let (cells, reserved_ptr, reserved_capacity, cell_count) = self.materialize_cell_vec();
        assert_eq!(
            cells.len(),
            cell_count,
            "clustered line materialization diverged from its bounded width census"
        );
        assert_eq!(
            cells.as_ptr(),
            reserved_ptr,
            "plaintext-bearing Cell buffer reallocated during materialization"
        );
        assert_eq!(
            cells.capacity(),
            reserved_capacity,
            "plaintext-bearing Cell buffer capacity changed during materialization"
        );
        cells
    }

    pub fn from_cell_vec<'a>(hint: usize, iter: impl Iterator<Item = CellRef<'a>>) -> Self {
        let mut last_cluster: Option<Cluster> = None;
        let mut is_double_wide = FixedBitSet::with_capacity(hint);
        // Attribute cloning and cluster growth happen after text append in the
        // loop below and may unwind.  Guard the builder from its first
        // allocation until the complete ClusteredLine can take ownership.
        let mut text = Zeroizing::new(String::new());
        let mut clusters = vec![];
        let mut any_double = false;
        let mut len = 0usize;
        let mut last_cell_width = None;

        for cell in iter {
            let cell_width = Self::normalize_cell_width(cell.width());
            len = len.saturating_add(usize::from(cell_width));
            last_cell_width = NonZeroU8::new(cell_width as u8);

            if cell_width > 1 {
                any_double = true;
                is_double_wide.set(cell.cell_index(), true);
            }

            guarded_push_str(&mut text, cell.str());

            last_cluster = match last_cluster.take() {
                None => Some(Cluster {
                    cell_width,
                    attrs: cell.attrs().clone(),
                }),
                Some(cluster) if cluster.attrs != *cell.attrs() => {
                    clusters.push(cluster);
                    Some(Cluster {
                        cell_width,
                        attrs: cell.attrs().clone(),
                    })
                }
                Some(mut cluster) => match cluster.cell_width.checked_add(cell_width) {
                    Some(width) => {
                        cluster.cell_width = width;
                        Some(cluster)
                    }
                    None => {
                        clusters.push(cluster);
                        Some(Cluster {
                            cell_width,
                            attrs: cell.attrs().clone(),
                        })
                    }
                },
            };
        }

        if let Some(cluster) = last_cluster.take() {
            clusters.push(cluster);
        }

        // Box allocation is the final potentially panicking construction
        // step; complete it while the accumulated text is still guarded.
        let is_double_wide = if any_double {
            Some(Box::new(is_double_wide))
        } else {
            None
        };

        Self {
            text: core::mem::take(&mut *text),
            is_double_wide,
            clusters,
            len: len.min(u32::MAX as usize) as u32,
            last_cell_width,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Whether a cell holding the grapheme `text` may be appended at cell
    /// `idx` (at or past the end; blanks fill any gap first) so that the
    /// concatenated text still segments into exactly this line's cells.
    ///
    /// Iteration re-runs `Graphemes` over the concatenated text, while `len`
    /// counts the widths appended. Appending a regional indicator after its
    /// partner, a skin-tone modifier after an emoji, or anything else that
    /// clusters with the previous cell would merge the two on iteration, so
    /// `len` would exceed the cells the line yields. Callers then index past
    /// the materialized cells (DCH after a split flag emoji panicked in
    /// `Line::erase_cell_with_margin`). Such writes must use vector storage.
    pub fn can_append_cell_at(&self, idx: usize, text: &str) -> bool {
        let len = self.len();
        if idx < len {
            return false;
        }
        let first = match text.chars().next() {
            Some(first) => first,
            None => return false,
        };
        let mut prev = self.text.chars().next_back();
        if idx > len {
            if !breaks_between(prev, ' ') {
                return false;
            }
            prev = Some(' ');
        }
        breaks_between(prev, first)
    }

    /// Whether iterating this line yields exactly `cells`: the same
    /// graphemes with the same widths, in order. A clustered line built from
    /// cells whose neighbours cluster with each other would not.
    ///
    /// `cells` yields a fresh iterator per pass: a cheap pass checks that
    /// every boundary is inert, and only a failure pays for re-segmenting.
    pub fn reproduces<'a, I>(&self, cells: impl Fn() -> I) -> bool
    where
        I: Iterator<Item = CellRef<'a>>,
    {
        let mut prev = None;
        let mut inert = true;
        for cell in cells() {
            let text = cell.str();
            match text.chars().next() {
                Some(first) if breaks_between(prev, first) => {}
                _ => {
                    inert = false;
                    break;
                }
            }
            prev = text.chars().next_back();
        }
        if inert {
            return true;
        }
        let mut own = self.iter();
        for cell in cells() {
            match own.next() {
                Some(mine)
                    if mine.str() == cell.str()
                        && mine.width()
                            == usize::from(Self::normalize_cell_width(cell.width())) => {}
                _ => return false,
            }
        }
        own.next().is_none()
    }

    fn is_double_wide(&self, cell_index: usize) -> bool {
        match &self.is_double_wide {
            Some(bitset) => bitset.contains(cell_index),
            None => false,
        }
    }

    pub fn iter(&self) -> ClusterLineCellIter<'_> {
        let mut clusters = self.clusters.iter();
        let cluster = clusters.next();
        ClusterLineCellIter {
            graphemes: Graphemes::new(&self.text),
            clusters,
            cluster,
            idx: 0,
            cluster_total: 0,
            line: self,
        }
    }

    pub fn append_grapheme(&mut self, text: &str, cell_width: usize, attrs: CellAttributes) {
        let cell_width = Self::normalize_cell_width(cell_width);
        guarded_reserve_text(&mut self.text, text.len());
        let new_cluster = match self.clusters.last() {
            Some(cluster) => {
                if cluster.attrs != attrs {
                    true
                } else {
                    // If we overflow the max length of a run,
                    // then we need a new cluster
                    let (_, did_overflow) = cluster.cell_width.overflowing_add(cell_width);
                    did_overflow
                }
            }
            None => true,
        };
        let new_cell_index = self.len as usize;
        if new_cluster {
            self.clusters.push(Cluster { attrs, cell_width });
        } else if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width += cell_width;
        }
        self.text.push_str(text);
        if cell_width > 1 {
            let bitset = match self.is_double_wide.take() {
                Some(mut bitset) => {
                    bitset.grow(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    bitset
                }
                None => {
                    let mut bitset = FixedBitSet::with_capacity(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    Box::new(bitset)
                }
            };
            self.is_double_wide.replace(bitset);
        }
        self.last_cell_width = NonZeroU8::new(cell_width as u8);
        self.len = self.len.saturating_add(u32::from(cell_width));
    }

    pub fn append_ascii_run(&mut self, text: &str, attrs: CellAttributes) {
        debug_assert!(text.is_ascii());
        if text.is_empty() {
            return;
        }
        guarded_reserve_text(&mut self.text, text.len());
        self.extend_clusters(text.len(), &attrs);
        self.text.push_str(text);
        self.last_cell_width = NonZeroU8::new(1);
        self.len = self
            .len
            .saturating_add(text.len().min(u32::MAX as usize) as u32);
    }

    /// Appends `count` blank cells: what `append_grapheme(" ", 1,
    /// CellAttributes::blank())` does `count` times, in one step
    /// (ft-70s4z). Writing a cell past the end of a line fills the gap with
    /// these, and a cell at a far column used to pay a detach, a capacity
    /// check and an attribute comparison per blank.
    pub fn append_blank_cells(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        guarded_reserve_text(&mut self.text, count);
        self.extend_clusters(count, &CellAttributes::blank());
        self.text.extend(core::iter::repeat_n(' ', count));
        self.last_cell_width = NonZeroU8::new(1);
        self.len = self.len.saturating_add(count.min(u32::MAX as usize) as u32);
    }

    /// Accounts `count` width-1 cells with `attrs` to the attribute runs:
    /// the last run grows while its attributes match and it has room, then
    /// new runs of at most `u16::MAX` cells follow. One cell at a time
    /// through `append_grapheme` builds exactly the same runs.
    fn extend_clusters(&mut self, count: usize, attrs: &CellAttributes) {
        const MAX_CLUSTER_CELL_WIDTH: usize = u16::MAX as usize;
        let mut remaining = count;
        while remaining > 0 {
            let appended_to_last = match self.clusters.last_mut() {
                Some(cluster) if cluster.attrs == *attrs => {
                    let available =
                        MAX_CLUSTER_CELL_WIDTH.saturating_sub(cluster.cell_width as usize);
                    let take = remaining.min(available);
                    cluster.cell_width += take as u16;
                    take
                }
                _ => 0,
            };

            if appended_to_last > 0 {
                remaining -= appended_to_last;
                continue;
            }

            let take = remaining.min(MAX_CLUSTER_CELL_WIDTH);
            self.clusters.push(Cluster {
                cell_width: take as u16,
                attrs: attrs.clone(),
            });
            remaining -= take;
        }
    }

    pub fn append(&mut self, cell: Cell) {
        let cell_width = Self::normalize_cell_width(cell.width());
        guarded_reserve_text(&mut self.text, cell.str().len());
        let new_cluster = match self.clusters.last() {
            Some(cluster) => {
                if cluster.attrs != *cell.attrs() {
                    true
                } else {
                    // If we overflow the max length of a run,
                    // then we need a new cluster
                    let (_, did_overflow) = cluster.cell_width.overflowing_add(cell_width);
                    did_overflow
                }
            }
            None => true,
        };
        let new_cell_index = self.len as usize;
        if new_cluster {
            self.clusters.push(Cluster {
                attrs: (*cell.attrs()).clone(),
                cell_width,
            });
        } else if let Some(cluster) = self.clusters.last_mut() {
            cluster.cell_width += cell_width;
        }
        self.text.push_str(cell.str());
        if cell_width > 1 {
            let bitset = match self.is_double_wide.take() {
                Some(mut bitset) => {
                    bitset.grow(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    bitset
                }
                None => {
                    let mut bitset = FixedBitSet::with_capacity(new_cell_index + 1);
                    bitset.set(new_cell_index, true);
                    Box::new(bitset)
                }
            };
            self.is_double_wide.replace(bitset);
        }
        self.last_cell_width = NonZeroU8::new(cell_width as u8);
        self.len = self.len.saturating_add(u32::from(cell_width));
    }

    pub fn prune_trailing_blanks(&mut self) -> bool {
        let num_spaces = self.text.chars().rev().take_while(|&c| c == ' ').count();
        if num_spaces == 0 {
            return false;
        }

        let blank = CellAttributes::blank();
        let mut pruned = false;
        for _ in 0..num_spaces {
            let current_len = self.len as usize;
            let cell_width = if current_len >= 2 && self.is_double_wide(current_len - 2) {
                2
            } else {
                1
            };
            let Some(new_len) = current_len.checked_sub(cell_width) else {
                break;
            };
            if self.text.as_bytes().last() != Some(&b' ') {
                break;
            }
            let Some(cluster) = self.clusters.last_mut() else {
                break;
            };
            if cluster.attrs != blank || cluster.cell_width < cell_width as u16 {
                break;
            }

            cluster.cell_width -= cell_width as u16;
            let need_pop = cluster.cell_width == 0;
            self.text.pop();
            self.len -= cell_width as u32;
            if cell_width == 2 {
                if let Some(bitset) = self.is_double_wide.as_mut() {
                    bitset.set(new_len, false);
                }
            }
            self.last_cell_width.take();
            pruned = true;
            if need_pop {
                self.clusters.pop();
            }
        }

        if self
            .is_double_wide
            .as_ref()
            .is_some_and(|bitset| bitset.is_clear())
        {
            self.is_double_wide.take();
        }

        pruned
    }

    fn compute_last_cell_width(&mut self) -> Option<NonZeroU8> {
        if self.last_cell_width.is_none() {
            if let Some(last_cell) = self.iter().last() {
                self.last_cell_width = NonZeroU8::new(last_cell.width() as u8);
            }
        }
        self.last_cell_width
    }

    pub fn last_cell_was_wrapped(&self) -> bool {
        // Canonical clusters cover the text in order, so the final run owns
        // the final visible cell's attributes. No grapheme scan is necessary.
        !self.text.is_empty()
            && self
                .clusters
                .last()
                .is_some_and(|cluster| cluster.attrs.wrapped())
    }

    pub fn set_last_cell_was_wrapped(&mut self, wrapped: bool) {
        if let Some(width) = self.compute_last_cell_width() {
            let width = width.get() as u16;
            if let Some(last_cluster) = self.clusters.last_mut() {
                let mut attrs = last_cluster.attrs.clone();
                attrs.set_wrapped(wrapped);

                if last_cluster.cell_width == width {
                    // Re-purpose final cluster
                    last_cluster.attrs = attrs;
                } else {
                    last_cluster.cell_width -= width;
                    self.clusters.push(Cluster {
                        cell_width: width,
                        attrs,
                    });
                }
            }
        }
    }

    fn wipe_owned_text(&mut self) {
        self.text.zeroize();
    }
}

impl Drop for ClusteredLine {
    fn drop(&mut self) {
        self.wipe_owned_text();
    }
}

impl zeroize::ZeroizeOnDrop for ClusteredLine {}

impl Clone for ClusteredLine {
    fn clone(&self) -> Self {
        // A derived clone materializes raw text before cloning later fields.
        // Keep that text guarded until every potentially allocating clone has
        // succeeded and the final Drop-hardened line can take ownership.
        let mut text = Zeroizing::new(self.text.clone());
        let is_double_wide = self.is_double_wide.clone();
        let clusters = self.clusters.clone();
        Self {
            text: core::mem::take(&mut *text),
            is_double_wide,
            clusters,
            len: self.len,
            last_cell_width: self.last_cell_width,
        }
    }
}

impl ClusteredLine {
    /// Image presence belongs to attributes, which are constant across each
    /// cluster. Do not decode every grapheme to inspect the same attributes.
    #[cfg(feature = "use_image")]
    pub(crate) fn has_image_attachments(&self) -> bool {
        self.clusters
            .iter()
            .any(|cluster| cluster.attrs.has_image_attachments())
    }

    pub(crate) fn snapshot_clone_cost(&self, max_clusters: usize) -> Option<(usize, usize)> {
        if self.clusters.len() > max_clusters {
            return None;
        }
        let mut bytes = self.text.len().checked_add(
            self.clusters
                .len()
                .checked_mul(core::mem::size_of::<Cluster>())?,
        )?;
        if let Some(mask) = &self.is_double_wide {
            bytes = bytes
                .checked_add(core::mem::size_of::<FixedBitSet>())?
                // fixedbitset 0.5.7 Clone copies SIMD blocks (up to32 bytes),
                // while as_slice exposes only logical usize words.
                .checked_add(
                    core::mem::size_of_val(mask.as_slice())
                        .div_ceil(32)
                        .checked_mul(32)?,
                )?;
        }
        for cluster in &self.clusters {
            bytes = bytes.checked_add(cluster.attrs.snapshot_clone_heap_bytes()?)?;
        }
        Some((bytes, self.clusters.len()))
    }
}

pub(crate) struct ClusterLineCellIter<'a> {
    graphemes: Graphemes<'a>,
    clusters: core::slice::Iter<'a, Cluster>,
    cluster: Option<&'a Cluster>,
    idx: usize,
    cluster_total: usize,
    line: &'a ClusteredLine,
}

impl<'a> Iterator for ClusterLineCellIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<CellRef<'a>> {
        let text = self.graphemes.next()?;

        let cell_index = self.idx;
        let width = if self.line.is_double_wide(cell_index) {
            2
        } else {
            1
        };
        self.idx += width;
        self.cluster_total += width;
        let attrs = &self.cluster.as_ref()?.attrs;

        if self.cluster_total >= self.cluster.as_ref()?.cell_width as usize {
            self.cluster = self.clusters.next();
            self.cluster_total = 0;
        }

        Some(CellRef::ClusterRef {
            cell_index,
            width,
            text,
            attrs,
        })
    }
}

#[cfg(test)]
mod test {
    #[test]
    fn snapshot_clone_charges_simd_rounding_for_width_mask() {
        let mut line = super::ClusteredLine::new();
        line.is_double_wide = Some(Box::new(fixedbitset::FixedBitSet::with_capacity(1)));
        let (bytes, visits) = line.snapshot_clone_cost(0).unwrap();
        assert_eq!(visits, 0);
        assert_eq!(bytes, core::mem::size_of::<fixedbitset::FixedBitSet>() + 32);
    }
    use super::*;
    use alloc::string::ToString;

    #[cfg(feature = "use_serde")]
    #[test]
    fn wide_mask_wire_omits_unused_trailing_extent() {
        let source = crate::line::Line::from_text("a界 ", &CellAttributes::blank(), 1, None);
        let line = ClusteredLine::from_cell_vec(source.len(), source.visible_cells());
        let wire = serde_json::to_value(&line).unwrap();
        let restored: ClusteredLine = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(line.is_double_wide.as_ref().unwrap().len(), 4);
        assert_eq!(restored.is_double_wide.as_ref().unwrap().len(), 2);
        assert_ne!(
            line, restored,
            "derived equality retains exact bitset extent"
        );
        assert_eq!(serde_json::to_value(&restored).unwrap(), wire);
        assert_eq!(line.to_cell_vec(), restored.to_cell_vec());
        assert_eq!(line.len(), restored.len());
        assert_eq!(
            line.last_cell_was_wrapped(),
            restored.last_cell_was_wrapped()
        );
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn memory_usage() {
        assert_eq!(core::mem::size_of::<ClusteredLine>(), 64);
        assert_eq!(core::mem::size_of::<String>(), 24);
        assert_eq!(core::mem::size_of::<Vec<Cluster>>(), 24);
        assert_eq!(core::mem::size_of::<Option<Box<FixedBitSet>>>(), 8);
        assert_eq!(core::mem::size_of::<Option<NonZeroU8>>(), 1);
    }

    #[test]
    fn append_grapheme_normalizes_zero_and_extreme_widths() {
        let mut line = ClusteredLine::new();
        line.append_grapheme("a", 0, CellAttributes::default());
        line.append_grapheme("b", usize::MAX, CellAttributes::default());

        assert_eq!(line.len(), 3);
        let cells: Vec<_> = line
            .iter()
            .map(|cell| (cell.cell_index(), cell.str().to_string(), cell.width()))
            .collect();
        assert_eq!(
            cells,
            vec![(0, "a".to_string(), 1), (1, "b".to_string(), 2)]
        );
        assert_eq!(line.clusters[0].cell_width, 3);
    }

    #[test]
    fn from_cell_vec_splits_cluster_runs_before_u16_overflow() {
        let cells = vec![Cell::new_grapheme_with_width("x", 2, CellAttributes::default()); 40_000];
        let line = ClusteredLine::from_cell_vec(
            cells.len() * 2,
            cells
                .iter()
                .enumerate()
                .map(|(idx, cell)| CellRef::CellRef {
                    cell_index: idx * 2,
                    cell,
                }),
        );

        assert_eq!(line.len(), 80_000);
        assert!(
            line.clusters.iter().all(|cluster| cluster.cell_width > 0),
            "cluster run widths must never wrap to zero"
        );
        assert!(
            line.clusters.len() > 1,
            "same-attribute runs must split before u16 overflow"
        );
        assert_eq!(
            line.iter().map(|cell| cell.width()).sum::<usize>(),
            line.len()
        );
    }

    #[test]
    fn prune_trailing_blanks_removes_the_full_width_of_a_wide_space() {
        let mut line = ClusteredLine::new();
        line.append_grapheme("\u{4e2d}", 2, CellAttributes::default());
        line.append_grapheme(" ", 2, CellAttributes::default());
        assert_eq!(line.len(), 4);

        assert!(line.prune_trailing_blanks());

        assert_eq!(line.len(), 2);
        assert_eq!(line.text, "\u{4e2d}");
        assert_eq!(
            line.iter()
                .map(|cell| (cell.cell_index(), cell.str().to_string(), cell.width()))
                .collect::<Vec<_>>(),
            vec![(0, "\u{4e2d}".to_string(), 2)],
        );
        assert_eq!(line.clusters[0].cell_width, 2);
        assert_eq!(
            line.is_double_wide
                .as_ref()
                .map(|bitset| bitset.ones().collect::<Vec<_>>()),
            Some(vec![0]),
        );

        let mut only_wide_space = ClusteredLine::new();
        only_wide_space.append_grapheme(" ", 2, CellAttributes::default());
        assert!(only_wide_space.prune_trailing_blanks());
        assert_eq!(only_wide_space.len(), 0);
        assert!(only_wide_space.text.is_empty());
        assert!(only_wide_space.clusters.is_empty());
        assert!(only_wide_space.is_double_wide.is_none());
        assert_eq!(only_wide_space.iter().count(), 0);
    }

    #[test]
    fn to_cell_vec_does_not_reallocate_the_materialized_cell_buffer() {
        let mut line = ClusteredLine::new();
        line.append_grapheme("a", 1, CellAttributes::default());
        line.append_grapheme("\u{4e2d}", 2, CellAttributes::default());

        let (cells, reserved_ptr, reserved_capacity, cell_count) = line.materialize_cell_vec();

        assert_eq!(cells.len(), 3);
        assert_eq!(cells.len(), cell_count);
        assert_eq!(cells.as_ptr(), reserved_ptr);
        assert_eq!(
            cells.capacity(),
            reserved_capacity,
            "materialization must not reallocate its plaintext-bearing Cell buffer"
        );
    }

    #[test]
    fn clustered_line_wipes_owned_text_in_place() {
        fn require_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
        require_zeroize_on_drop::<ClusteredLine>();

        let mut line = ClusteredLine::new();
        line.append_ascii_run("semantic terminal text", CellAttributes::default());
        let capacity = line.text.capacity();

        line.wipe_owned_text();

        assert!(line.text.is_empty());
        assert_eq!(line.text.capacity(), capacity);
    }

    #[test]
    fn clustered_line_clone_owns_an_independent_text_allocation() {
        let mut line = ClusteredLine::new();
        line.append_ascii_run("semantic terminal text", CellAttributes::default());
        let source_text = line.text.as_ptr();
        let cloned = line.clone();

        assert_ne!(source_text, cloned.text.as_ptr());
        drop(line);
        assert_eq!(cloned.text, "semantic terminal text");
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn serde_failure_after_text_decoding_wipes_the_guarded_text() {
        let before = GUARDED_LINE_TEXT_WIPE_INVOCATIONS.load(core::sync::atomic::Ordering::Relaxed);
        let result = serde_json::from_str::<ClusteredLine>(
            r#"{"text":"semantic terminal text","is_double_wide":[],"clusters":"not-an-array","len":0,"last_cell_width":null}"#,
        );

        assert!(result.is_err());
        assert!(
            GUARDED_LINE_TEXT_WIPE_INVOCATIONS.load(core::sync::atomic::Ordering::Relaxed) > before,
            "a later-field serde error must drop and wipe the decoded text guard"
        );
    }

    /// ft-70s4z: `append_blank_cells(n)` builds exactly the line `n` single
    /// blank `append_grapheme` calls build, after every kind of tail: none,
    /// a blank run, another attribute, a wide cell, and a run one cell short
    /// of `u16::MAX` (so the bulk fill crosses into new runs).
    #[test]
    fn append_blank_cells_matches_one_blank_at_a_time() {
        let mut bold = CellAttributes::blank();
        bold.set_intensity(frankenterm_cell::Intensity::Bold);
        let mut starts = vec![ClusteredLine::new()];
        let mut blank_tail = ClusteredLine::new();
        blank_tail.append_ascii_run("ab ", CellAttributes::blank());
        starts.push(blank_tail);
        let mut bold_tail = ClusteredLine::new();
        bold_tail.append_ascii_run("abc", bold.clone());
        starts.push(bold_tail);
        let mut wide_tail = ClusteredLine::new();
        wide_tail.append_grapheme("\u{4e2d}", 2, bold);
        starts.push(wide_tail);
        let mut nearly_full = ClusteredLine::new();
        for _ in 0..u16::MAX - 1 {
            nearly_full.append_grapheme(" ", 1, CellAttributes::blank());
        }
        starts.push(nearly_full);

        for (which, start) in starts.iter().enumerate() {
            for count in [0usize, 1, 2, 7, 70_000] {
                let mut bulk = start.clone();
                bulk.append_blank_cells(count);
                let mut single = start.clone();
                for _ in 0..count {
                    single.append_grapheme(" ", 1, CellAttributes::blank());
                }
                assert!(
                    bulk == single,
                    "start {} count {}: bulk {} runs, single {} runs",
                    which,
                    count,
                    bulk.clusters.len(),
                    single.clusters.len()
                );
            }
        }
    }
}
