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
#[derive(Debug, PartialEq)]
pub(crate) struct VecStorage {
    // Render snapshots and cached reflows usually change only Line metadata.
    // Share their cell payload until a caller actually edits it.
    cells: Arc<Vec<Cell>>,
}

impl Clone for VecStorage {
    fn clone(&self) -> Self {
        // Retain the eager-copy arm for paired native profiling and a narrow
        // operational rollback. Resolve it once, never during individual cell
        // edits; both arms have identical serialization and mutation semantics.
        #[cfg(feature = "std")]
        {
            static EAGER_COPY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            if *EAGER_COPY.get_or_init(|| {
                std::env::var_os("FT_DISABLE_SHARED_LINE_CELLS").is_some_and(|v| v == "1")
            }) {
                return Self::new(self.cells.as_ref().clone());
            }
        }
        Self {
            cells: Arc::clone(&self.cells),
        }
    }
}

impl VecStorage {
    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self {
            cells: Arc::new(cells),
        }
    }

    #[cfg_attr(not(feature = "use_image"), allow(unused_mut, unused_variables))]
    pub(crate) fn set_cell(&mut self, idx: usize, mut cell: Cell, clear_image_placement: bool) {
        #[cfg(feature = "use_image")]
        if !clear_image_placement {
            if let Some(images) = self.cells[idx].attrs().images() {
                for image in images {
                    if image.has_placement_id() {
                        cell.attrs_mut().attach_image(Box::new(image));
                    }
                }
            }
        }
        Arc::make_mut(&mut self.cells)[idx] = cell;
    }

    pub(crate) fn scan_and_create_hyperlinks(&mut self, matches: Vec<HyperlinkCellMatch>) -> bool {
        let mut has_implicit_hyperlinks = false;
        for matched in matches {
            for cell_idx in matched.cell_indices {
                let Some(cell) = Arc::make_mut(&mut self.cells).get_mut(cell_idx) else {
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
        &self.cells
    }
}

impl core::ops::DerefMut for VecStorage {
    fn deref_mut(&mut self) -> &mut Vec<Cell> {
        Arc::make_mut(&mut self.cells)
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
        let vs2 = vs.clone();
        assert_eq!(vs, vs2);
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
