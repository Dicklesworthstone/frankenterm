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
use zeroize::Zeroize;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
struct Cluster {
    cell_width: u16,
    attrs: CellAttributes,
}

/// Stores line data as a contiguous string and a series of
/// clusters of attribute data describing attributed ranges
/// within the line
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
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

    pub fn to_cell_vec(&self) -> Vec<Cell> {
        // A growing Vec bitwise-moves initialized Cells into each replacement
        // allocation.  Inline TeenyString bytes would then remain in the old
        // allocation without running Cell::drop.  Count the exact materialized
        // width first so the plaintext-bearing Cell buffer never reallocates.
        let cell_count = self.iter().map(|cell| cell.width()).sum();
        let mut cells = Vec::with_capacity(cell_count);

        for c in self.iter() {
            cells.push(c.as_cell());
            for _ in 1..c.width() {
                cells.push(Cell::blank_with_attrs(c.attrs().clone()));
            }
        }

        cells
    }

    pub fn from_cell_vec<'a>(hint: usize, iter: impl Iterator<Item = CellRef<'a>>) -> Self {
        let mut last_cluster: Option<Cluster> = None;
        let mut is_double_wide = FixedBitSet::with_capacity(hint);
        let mut text = String::new();
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

            text.push_str(cell.str());

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

        Self {
            text,
            is_double_wide: if any_double {
                Some(Box::new(is_double_wide))
            } else {
                None
            },
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
    fn to_cell_vec_allocates_the_exact_materialized_cell_count() {
        let mut line = ClusteredLine::new();
        line.append_grapheme("a", 1, CellAttributes::default());
        line.append_grapheme("\u{4e2d}", 2, CellAttributes::default());

        let cells = line.to_cell_vec();

        assert_eq!(cells.len(), 3);
        assert_eq!(
            cells.capacity(),
            cells.len(),
            "materialization must not grow a plaintext-bearing Cell buffer"
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
}
