use crate::line::cellref::CellRef;
use alloc::sync::Arc;
use frankenterm_cell::Cell;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;

extern crate alloc;
use alloc::vec::Vec;

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VecStorage {
    cells: Vec<Cell>,
}

impl VecStorage {
    pub(crate) fn new(cells: Vec<Cell>) -> Self {
        Self { cells }
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
        self.cells[idx] = cell;
    }

    pub(crate) fn scan_and_create_hyperlinks(
        &mut self,
        line: &str,
        matches: Vec<crate::hyperlink::RuleMatch>,
    ) -> bool {
        // The capture range is measured in bytes but we need to translate
        // that to the index of the column.  This is complicated a bit further
        // because double wide sequences have a blank column cell after them
        // in the cells array, but the string we match against excludes that
        // string.
        let mut cell_idx = 0;
        let mut has_implicit_hyperlinks = false;
        for (byte_idx, _grapheme) in line.grapheme_indices(true) {
            let cell = &mut self.cells[cell_idx];
            let mut matched = false;
            for m in &matches {
                if m.range.contains(&byte_idx) {
                    let attrs = cell.attrs_mut();
                    // Don't replace existing links
                    if attrs.hyperlink().is_none() {
                        attrs.set_hyperlink(Some(Arc::clone(&m.link)));
                        matched = true;
                    }
                }
            }
            cell_idx += cell.width();
            if matched {
                has_implicit_hyperlinks = true;
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
        &mut self.cells
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
