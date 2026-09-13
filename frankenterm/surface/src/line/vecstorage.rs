use crate::line::cellref::CellRef;
use alloc::sync::Arc;
use frankenterm_cell::Cell;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};

extern crate alloc;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HyperlinkCellMatch {
    pub(crate) cell_indices: Vec<usize>,
    pub(crate) link: Arc<crate::hyperlink::Hyperlink>,
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub(crate) struct VecStorage {
    // Render snapshots and cached reflows usually change only Line metadata.
    // Share their cell payload until a caller actually edits it.
    cells: Arc<CellBuffer>,
}

#[cfg_attr(feature = "use_serde", derive(Deserialize))]
#[cfg_attr(feature = "use_serde", serde(transparent))]
#[derive(Clone)]
struct CellBuffer {
    cells: Vec<Cell>,
    #[cfg(feature = "std")]
    #[cfg_attr(feature = "use_serde", serde(skip))]
    deferred: Option<DeferredRow>,
    // All cell mutations pass through this type. Keeping the cache beside
    // the payload makes invalidation independent of callers' sequence numbers.
    #[cfg(feature = "std")]
    #[cfg_attr(feature = "use_serde", serde(skip))]
    // None marks image-bearing buffers: image payloads can mutate through a
    // shared handle without any cell edit, so their hashes must stay live.
    shape_hash: std::sync::OnceLock<Option<(u16, [u8; 16])>>,
    #[cfg(feature = "std")]
    #[cfg_attr(feature = "use_serde", serde(skip))]
    wrap_boundary: std::sync::OnceLock<bool>,
}

#[cfg(feature = "std")]
#[derive(Clone)]
struct DeferredRow {
    tokens: Arc<[Cell]>,
    range: core::ops::Range<usize>,
    tail: Option<Cell>,
    len: usize,
    has_images: bool,
    materialized: std::sync::OnceLock<Vec<Cell>>,
}

#[cfg(feature = "std")]
impl DeferredRow {
    fn iter(&self) -> TokenRowIter<'_> {
        TokenRowIter {
            cells: self.tokens[self.range.clone()].iter(),
            tail: self.tail.as_ref(),
            idx: 0,
        }
    }

    fn build_cells(&self) -> Vec<Cell> {
        let mut cells = Vec::with_capacity(self.len);
        for token in self.iter() {
            cells.push(token.as_cell());
            for _ in 1..token.width() {
                cells.push(Cell::blank_with_attrs(token.attrs().clone()));
            }
        }
        cells
    }
}

impl CellBuffer {
    fn materialized(&self) -> &Vec<Cell> {
        #[cfg(feature = "std")]
        if let Some(row) = &self.deferred {
            return row.materialized.get_or_init(|| row.build_cells());
        }
        &self.cells
    }
}

#[cfg(feature = "use_serde")]
impl Serialize for CellBuffer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Preserve the original transparent cell-array wire representation.
        self.materialized().serialize(serializer)
    }
}

impl core::fmt::Debug for VecStorage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VecStorage")
            .field("cells", self.cells.materialized())
            .finish()
    }
}

impl PartialEq for VecStorage {
    fn eq(&self, other: &Self) -> bool {
        // Snapshot validation is normally comparing the same immutable cell
        // allocation. All edits detach through Arc::make_mut, so this skips a
        // full row scan without weakening equality after either side changes.
        self.shares_cells_with(other) || self.cells.materialized() == other.cells.materialized()
    }
}

impl Clone for VecStorage {
    fn clone(&self) -> Self {
        // Retain the eager-copy arm for paired native profiling and a narrow
        // operational rollback. Resolve it once, never during individual cell
        // edits; both arms have identical serialization and mutation semantics.
        #[cfg(feature = "std")]
        let cells = {
            static EAGER_COPY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *EAGER_COPY.get_or_init(|| {
                std::env::var_os("FT_DISABLE_SHARED_LINE_CELLS").is_some_and(|v| v == "1")
            }) {
                Arc::new(CellBuffer {
                    cells: self.cells.materialized().clone(),
                    deferred: None,
                    shape_hash: self.cells.shape_hash.clone(),
                    wrap_boundary: self.cells.wrap_boundary.clone(),
                })
            } else {
                Arc::clone(&self.cells)
            }
        };
        #[cfg(not(feature = "std"))]
        let cells = Arc::clone(&self.cells);
        Self { cells }
    }
}

impl VecStorage {
    pub(crate) fn snapshot_clone(&self) -> Self {
        Self {
            cells: Arc::clone(&self.cells),
        }
    }

    pub(crate) fn shares_cells_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cells, &other.cells)
    }

    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self {
            cells: Arc::new(CellBuffer {
                cells,
                #[cfg(feature = "std")]
                deferred: None,
                #[cfg(feature = "std")]
                shape_hash: std::sync::OnceLock::new(),
                #[cfg(feature = "std")]
                wrap_boundary: std::sync::OnceLock::new(),
            }),
        }
    }

    #[cfg(feature = "std")]
    pub(crate) fn from_token_range(
        tokens: Arc<[Cell]>,
        range: core::ops::Range<usize>,
        wrapped: bool,
    ) -> Self {
        let slice = &tokens[range.clone()];
        let mut len = 0usize;
        let mut has_images = false;
        for cell in slice {
            len = len
                .checked_add(cell.width().max(1))
                .expect("row width overflow");
            has_images |= cell.attrs().has_image_attachments();
        }
        let tail = if wrapped {
            slice.last().map(|cell| {
                let mut cell = cell.clone();
                cell.attrs_mut().set_wrapped(true);
                cell
            })
        } else {
            None
        };
        Self {
            cells: Arc::new(CellBuffer {
                cells: Vec::new(),
                deferred: Some(DeferredRow {
                    tokens,
                    range,
                    tail,
                    len,
                    has_images,
                    materialized: std::sync::OnceLock::new(),
                }),
                shape_hash: std::sync::OnceLock::new(),
                wrap_boundary: std::sync::OnceLock::new(),
            }),
        }
    }

    pub(crate) fn len(&self) -> usize {
        #[cfg(feature = "std")]
        if let Some(row) = &self.cells.deferred {
            return row.len;
        }
        self.cells.cells.len()
    }

    pub(crate) fn visible_cells(&self) -> CellViewIter<'_> {
        #[cfg(feature = "std")]
        if let Some(row) = &self.cells.deferred {
            return CellViewIter::Tokens(row.iter());
        }
        CellViewIter::Physical(VecStorageIter {
            cells: self.cells.cells.iter(),
            idx: 0,
            skip_width: 0,
        })
    }

    #[cfg(any(feature = "std", feature = "use_image"))]
    pub(crate) fn has_image_attachments(&self) -> bool {
        #[cfg(feature = "std")]
        if let Some(row) = &self.cells.deferred {
            return row.has_images;
        }
        // Include malformed spacer cells on the physical-storage path.
        self.cells
            .cells
            .iter()
            .any(|cell| cell.attrs().has_image_attachments())
    }

    #[cfg(all(test, feature = "std"))]
    pub(crate) fn is_deferred_unmaterialized(&self) -> bool {
        self.cells
            .deferred
            .as_ref()
            .is_some_and(|row| row.materialized.get().is_none())
    }

    pub(crate) fn cached_wrap_boundary(&self, compute: impl FnOnce() -> bool) -> bool {
        #[cfg(feature = "std")]
        {
            static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *DISABLED.get_or_init(|| {
                std::env::var_os("FT_DISABLE_LINE_WRAP_BOUNDARY_CACHE")
                    .is_some_and(|value| value == "1")
            }) {
                return compute();
            }
            *self.cells.wrap_boundary.get_or_init(compute)
        }
        #[cfg(not(feature = "std"))]
        compute()
    }

    #[cfg(feature = "std")]
    pub(crate) fn cached_shape_hash(
        &self,
        line_bits: u16,
        compute: impl FnOnce() -> [u8; 16],
    ) -> [u8; 16] {
        static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DISABLED.get_or_init(|| {
            std::env::var_os("FT_DISABLE_LINE_SHAPE_HASH_CACHE").is_some_and(|v| v == "1")
        }) {
            return compute();
        }
        if let Some(entry) = self.cells.shape_hash.get() {
            if let Some((bits, hash)) = entry {
                if *bits == line_bits {
                    return *hash;
                }
            }
            // Metadata can change without touching cells. Never reuse a hash
            // for different bits; retaining the first key bounds cache memory.
            return compute();
        }
        let cacheable = !self.has_image_attachments();
        let hash = compute();
        let _ = self
            .cells
            .shape_hash
            .set(cacheable.then_some((line_bits, hash)));
        hash
    }

    fn cells_mut(&mut self) -> &mut Vec<Cell> {
        let buffer = Arc::make_mut(&mut self.cells);
        #[cfg(feature = "std")]
        {
            if let Some(mut row) = buffer.deferred.take() {
                buffer.cells = row.materialized.take().unwrap_or_else(|| row.build_cells());
            }
            buffer.shape_hash.take();
            buffer.wrap_boundary.take();
        }
        &mut buffer.cells
    }

    #[cfg_attr(not(feature = "use_image"), allow(unused_mut, unused_variables))]
    pub(crate) fn set_cell(&mut self, idx: usize, mut cell: Cell, clear_image_placement: bool) {
        #[cfg(feature = "use_image")]
        if !clear_image_placement {
            #[cfg(feature = "std")]
            let may_have_images = self
                .cells
                .deferred
                .as_ref()
                .is_none_or(|row| row.has_images);
            #[cfg(not(feature = "std"))]
            let may_have_images = true;
            if may_have_images {
                if let Some(images) = self.cells.materialized()[idx].attrs().images() {
                    for image in images {
                        if image.has_placement_id() {
                            cell.attrs_mut().attach_image(Box::new(image));
                        }
                    }
                }
            }
        }
        self.cells_mut()[idx] = cell;
    }

    pub(crate) fn scan_and_create_hyperlinks(&mut self, matches: Vec<HyperlinkCellMatch>) -> bool {
        let mut has_implicit_hyperlinks = false;
        for matched in matches {
            for cell_idx in matched.cell_indices {
                let Some(cell) = self.cells_mut().get_mut(cell_idx) else {
                    continue;
                };
                let attrs = cell.attrs_mut();
                // Don't replace existing links.
                if attrs.hyperlink().is_none() {
                    attrs.set_hyperlink(Some(Arc::clone(&matched.link)));
                    has_implicit_hyperlinks = true;
                }
            }
        }

        has_implicit_hyperlinks
    }
}

impl core::ops::Deref for VecStorage {
    type Target = Vec<Cell>;

    fn deref(&self) -> &Vec<Cell> {
        self.cells.materialized()
    }
}

pub(crate) enum CellViewIter<'a> {
    Physical(VecStorageIter<'a>),
    #[cfg(feature = "std")]
    Tokens(TokenRowIter<'a>),
}

impl<'a> Iterator for CellViewIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Physical(iter) => iter.next(),
            #[cfg(feature = "std")]
            Self::Tokens(iter) => iter.next(),
        }
    }
}

#[cfg(feature = "std")]
pub(crate) struct TokenRowIter<'a> {
    cells: core::slice::Iter<'a, Cell>,
    tail: Option<&'a Cell>,
    idx: usize,
}

#[cfg(feature = "std")]
impl<'a> Iterator for TokenRowIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let original = self.cells.next()?;
        let cell = if self.cells.len() == 0 {
            self.tail.unwrap_or(original)
        } else {
            original
        };
        let cell_index = self.idx;
        self.idx += cell.width().max(1);
        Some(CellRef::CellRef { cell_index, cell })
    }
}

impl core::ops::DerefMut for VecStorage {
    fn deref_mut(&mut self) -> &mut Vec<Cell> {
        self.cells_mut()
    }
}

/// Iterates over a slice of Cell, yielding only visible cells
pub(crate) struct VecStorageIter<'a> {
    pub cells: core::slice::Iter<'a, Cell>,
    pub idx: usize,
    pub skip_width: usize,
}

impl<'a> Iterator for VecStorageIter<'a> {
    type Item = CellRef<'a>;

    fn next(&mut self) -> Option<CellRef<'a>> {
        while self.skip_width > 0 {
            self.skip_width -= 1;
            let _ = self.cells.next()?;
            self.idx += 1;
        }
        let cell = self.cells.next()?;
        let cell_index = self.idx;
        self.idx += 1;
        self.skip_width = cell.width().saturating_sub(1);
        Some(CellRef::CellRef { cell_index, cell })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;
    use frankenterm_cell::{Cell, CellAttributes};

    fn make_cells(s: &str) -> Vec<Cell> {
        s.chars()
            .map(|c| Cell::new(c, CellAttributes::default()))
            .collect()
    }

    #[cfg(feature = "std")]
    #[test]
    fn deferred_rows_preserve_indices_padding_and_detach_on_mutation() {
        for wrapped in [false, true] {
            let tokens: Arc<[Cell]> = vec![
                Cell::new_grapheme_with_width("e\u{301}", 0, CellAttributes::default()),
                Cell::new('x', CellAttributes::default()),
                Cell::new_grapheme_with_width("界", 2, CellAttributes::default()),
                Cell::new_grapheme_with_width("🚀", 3, CellAttributes::default()),
            ]
            .into();
            let mut expected = Vec::new();
            for (index, token) in tokens.iter().enumerate() {
                let mut token = token.clone();
                if wrapped && index + 1 == tokens.len() {
                    token.attrs_mut().set_wrapped(true);
                }
                let attrs = token.attrs().clone();
                let width = token.width();
                expected.push(token);
                for _ in 1..width {
                    expected.push(Cell::blank_with_attrs(attrs.clone()));
                }
            }
            let mut row = VecStorage::from_token_range(tokens.clone(), 0..tokens.len(), wrapped);
            let frozen = row.snapshot_clone();
            let eager = VecStorage::new(expected.clone());
            assert_eq!(row.len(), expected.len());
            let views: Vec<_> = row.visible_cells().collect();
            let eager_views: Vec<_> = eager.visible_cells().collect();
            assert_eq!(views.len(), eager_views.len());
            for (actual, expected) in views.iter().zip(eager_views.iter()) {
                assert_eq!(actual.cell_index(), expected.cell_index());
                assert!(actual.same_contents(expected));
            }
            assert!(!row.has_image_attachments());
            assert!(row.is_deferred_unmaterialized());
            row.set_cell(1, Cell::new('Z', CellAttributes::default()), false);
            assert!(!row.shares_cells_with(&frozen));
            // Editing a clone must not force or alter the original row.
            assert!(frozen.is_deferred_unmaterialized());
            assert_eq!(frozen.as_slice(), expected.as_slice());
            assert_eq!(row[1].str(), "Z");
        }
    }

    #[cfg(all(feature = "std", feature = "use_serde"))]
    #[test]
    fn deferred_rows_serialize_as_original_cell_arrays() {
        let tokens: Arc<[Cell]> = make_cells("wire").into();
        let row = VecStorage::from_token_range(tokens.clone(), 0..tokens.len(), false);
        assert!(row.is_deferred_unmaterialized());
        let expected = serde_json::json!({"cells": tokens.as_ref()});
        let actual = serde_json::to_value(&row).unwrap();
        assert_eq!(actual, expected);
        let restored: VecStorage = serde_json::from_value(actual).unwrap();
        assert_eq!(row, restored);
    }

    // ── VecStorage ─────────────────────────────────────────

    #[test]
    fn vec_storage_new() {
        let vs = VecStorage::new(make_cells("abc"));
        assert_eq!(vs.len(), 3);
    }

    #[test]
    fn vec_storage_empty() {
        let vs = VecStorage::new(vec![]);
        assert_eq!(vs.len(), 0);
        assert!(vs.is_empty());
    }

    #[cfg(feature = "std")]
    #[test]
    fn wrap_boundary_cache_detaches_and_invalidates_on_mutation() {
        let mut cells = VecStorage::new(make_cells("ab"));
        assert!(cells.cached_wrap_boundary(|| true));
        assert!(cells.cached_wrap_boundary(|| panic!("rescanned immutable cells")));
        let frozen = cells.snapshot_clone();
        cells.set_cell(0, Cell::blank(), false);
        assert!(!cells.cached_wrap_boundary(|| false));
        assert!(frozen.cached_wrap_boundary(|| panic!("snapshot cache lost")));
        cells.push(Cell::blank());
        assert!(cells.cached_wrap_boundary(|| true));
        cells[0].attrs_mut().set_wrapped(false);
        assert!(!cells.cached_wrap_boundary(|| false));
    }

    #[test]
    fn vec_storage_deref_access() {
        let vs = VecStorage::new(make_cells("hi"));
        // Deref to Vec<Cell>
        assert_eq!(vs[0].str(), "h");
        assert_eq!(vs[1].str(), "i");
    }

    #[test]
    fn vec_storage_deref_mut_push() {
        let mut vs = VecStorage::new(make_cells("ab"));
        vs.push(Cell::new('c', CellAttributes::default()));
        assert_eq!(vs.len(), 3);
    }

    #[test]
    fn vec_storage_set_cell() {
        let mut vs = VecStorage::new(make_cells("ab"));
        vs.set_cell(0, Cell::new('X', CellAttributes::default()), false);
        assert_eq!(vs[0].str(), "X");
        assert_eq!(vs[1].str(), "b");
    }

    #[test]
    fn vec_storage_clone_eq() {
        let vs = VecStorage::new(make_cells("test"));
        let mut vs2 = vs.clone();
        assert_eq!(vs, vs2);
        let independent = VecStorage::new(make_cells("test"));
        assert!(!vs.shares_cells_with(&independent));
        assert_eq!(vs, independent);
        vs2.set_cell(0, Cell::new('X', CellAttributes::default()), false);
        assert!(!vs.shares_cells_with(&vs2));
        assert_ne!(vs, vs2);
        vs2.set_cell(0, Cell::new('t', CellAttributes::default()), false);
        assert_eq!(vs, vs2);
    }

    #[cfg(feature = "std")]
    #[test]
    fn shape_hash_cache_reuses_reads_but_rejects_metadata_and_cell_changes() {
        let mut cells = VecStorage::new(make_cells("ab"));
        assert_eq!(cells.cached_shape_hash(0, || [1; 16]), [1; 16]);
        assert_eq!(cells.cached_shape_hash(0, || panic!("rehashed")), [1; 16]);
        assert_eq!(cells.cached_shape_hash(1, || [2; 16]), [2; 16]);
        assert_eq!(
            cells.cached_shape_hash(0, || panic!("lost original key")),
            [1; 16]
        );

        let snapshot = cells.clone();
        cells.set_cell(0, Cell::new('X', CellAttributes::default()), false);
        assert_eq!(cells.cached_shape_hash(0, || [3; 16]), [3; 16]);
        assert_eq!(
            snapshot.cached_shape_hash(0, || panic!("clone lost cache")),
            [1; 16]
        );
        cells.push(Cell::blank());
        assert_eq!(cells.cached_shape_hash(0, || [4; 16]), [4; 16]);
        cells.scan_and_create_hyperlinks(vec![HyperlinkCellMatch {
            cell_indices: vec![0],
            link: Arc::new(crate::hyperlink::Hyperlink::new("https://example.invalid")),
        }]);
        assert_eq!(cells.cached_shape_hash(0, || [5; 16]), [5; 16]);
    }

    #[test]
    fn vec_storage_snapshots_share_cells_until_mutation_and_remain_independent() {
        let mut original = VecStorage::new(make_cells("abcd"));
        let mut snapshot = original.clone();
        assert_eq!(original.as_ptr(), snapshot.as_ptr());

        original.set_cell(0, Cell::new('X', CellAttributes::default()), false);
        assert_ne!(original.as_ptr(), snapshot.as_ptr());
        assert_eq!(snapshot[0].str(), "a");
        snapshot.push(Cell::new('e', CellAttributes::default()));
        assert_eq!(original.len(), 4);
        assert_eq!(snapshot.len(), 5);

        let frozen = original.clone();
        original.scan_and_create_hyperlinks(vec![HyperlinkCellMatch {
            cell_indices: vec![1, 2],
            link: Arc::new(crate::hyperlink::Hyperlink::new("https://example.invalid")),
        }]);
        assert!(original[1].attrs().hyperlink().is_some());
        assert!(frozen[1].attrs().hyperlink().is_none());
        assert!(snapshot[1].attrs().hyperlink().is_none());
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn vec_storage_sharing_preserves_the_existing_serialized_cell_array() {
        let cells = make_cells("wire");
        let original = VecStorage::new(cells.clone());
        let snapshot = original.clone();
        let expected = serde_json::json!({"cells": cells});
        assert_eq!(serde_json::to_value(&original).unwrap(), expected);
        assert_eq!(serde_json::to_value(&snapshot).unwrap(), expected);
        let restored: VecStorage = serde_json::from_value(expected).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn vec_storage_ne() {
        let vs1 = VecStorage::new(make_cells("ab"));
        let vs2 = VecStorage::new(make_cells("cd"));
        assert_ne!(vs1, vs2);
    }

    #[test]
    fn vec_storage_debug() {
        let vs = VecStorage::new(make_cells("x"));
        let dbg = format!("{:?}", vs);
        assert!(dbg.contains("VecStorage"));
    }

    // ── VecStorageIter ─────────────────────────────────────

    #[test]
    fn vec_storage_iter_single_width() {
        let vs = VecStorage::new(make_cells("abc"));
        let iter = VecStorageIter {
            cells: vs.iter(),
            idx: 0,
            skip_width: 0,
        };
        let refs: Vec<_> = iter.collect();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].str(), "a");
        assert_eq!(refs[1].str(), "b");
        assert_eq!(refs[2].str(), "c");
    }

    #[test]
    fn vec_storage_iter_cell_indices() {
        let vs = VecStorage::new(make_cells("xy"));
        let iter = VecStorageIter {
            cells: vs.iter(),
            idx: 0,
            skip_width: 0,
        };
        let refs: Vec<_> = iter.collect();
        assert_eq!(refs[0].cell_index(), 0);
        assert_eq!(refs[1].cell_index(), 1);
    }

    #[test]
    fn vec_storage_iter_empty() {
        let vs = VecStorage::new(vec![]);
        let iter = VecStorageIter {
            cells: vs.iter(),
            idx: 0,
            skip_width: 0,
        };
        assert_eq!(iter.count(), 0);
    }

    #[test]
    fn vec_storage_iter_with_initial_skip() {
        let vs = VecStorage::new(make_cells("abc"));
        let iter = VecStorageIter {
            cells: vs.iter(),
            idx: 0,
            skip_width: 1, // skip first cell as if preceded by double-wide
        };
        let refs: Vec<_> = iter.collect();
        // Should skip 'a', then yield 'b' and 'c'
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].str(), "b");
        assert_eq!(refs[1].str(), "c");
    }
}
