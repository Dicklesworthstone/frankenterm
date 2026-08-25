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

        const MAX_CLUSTER_CELL_WIDTH: usize = u16::MAX as usize;
        let mut remaining = text;
        while !remaining.is_empty() {
            let appended_to_last = match self.clusters.last_mut() {
                Some(cluster) if cluster.attrs == attrs => {
                    let available =
                        MAX_CLUSTER_CELL_WIDTH.saturating_sub(cluster.cell_width as usize);
                    if available == 0 {
                        0
                    } else {
                        let take = remaining.len().min(available);
                        cluster.cell_width += take as u16;
                        take
                    }
                }
                _ => 0,
            };

            if appended_to_last > 0 {
                remaining = &remaining[appended_to_last..];
                continue;
            }

            let take = remaining.len().min(MAX_CLUSTER_CELL_WIDTH);
            self.clusters.push(Cluster {
                cell_width: take as u16,
                attrs: attrs.clone(),
            });
            remaining = &remaining[take..];
        }

        self.text.push_str(text);
        self.last_cell_width = NonZeroU8::new(1);
        self.len = self
            .len
            .saturating_add(text.len().min(u32::MAX as usize) as u32);
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
    use super::*;
    use alloc::string::ToString;

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
        let before = GUARDED_LINE_TEXT_WIPE_INVOCATIONS
            .load(core::sync::atomic::Ordering::Relaxed);
        let result = serde_json::from_str::<ClusteredLine>(
            r#"{"text":"semantic terminal text","is_double_wide":[],"clusters":"not-an-array","len":0,"last_cell_width":null}"#,
        );

        assert!(result.is_err());
        assert!(
            GUARDED_LINE_TEXT_WIPE_INVOCATIONS.load(core::sync::atomic::Ordering::Relaxed) > before,
            "a later-field serde error must drop and wipe the decoded text guard"
        );
    }
}
