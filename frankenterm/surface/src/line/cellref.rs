use core::hash::{Hash, Hasher};
use frankenterm_cell::{Cell, CellAttributes};
use frankenterm_char_props::emoji::Presentation;

#[derive(Debug, Clone, Copy)]
pub enum CellRef<'a> {
    CellRef {
        cell_index: usize,
        cell: &'a Cell,
    },
    ClusterRef {
        cell_index: usize,
        text: &'a str,
        width: usize,
        attrs: &'a CellAttributes,
    },
}

impl<'a> CellRef<'a> {
    pub fn cell_index(&self) -> usize {
        match self {
            Self::ClusterRef { cell_index, .. } | Self::CellRef { cell_index, .. } => *cell_index,
        }
    }

    pub fn str(&self) -> &str {
        match self {
            Self::CellRef { cell, .. } => cell.str(),
            Self::ClusterRef { text, .. } => text,
        }
    }

    pub fn width(&self) -> usize {
        match self {
            Self::CellRef { cell, .. } => cell.width(),
            Self::ClusterRef { width, .. } => *width,
        }
    }

    pub fn attrs(&self) -> &CellAttributes {
        match self {
            Self::CellRef { cell, .. } => cell.attrs(),
            Self::ClusterRef { attrs, .. } => attrs,
        }
    }

    pub fn presentation(&self) -> Presentation {
        match self {
            Self::CellRef { cell, .. } => cell.presentation(),
            Self::ClusterRef { text, .. } => match Presentation::for_grapheme(text) {
                (_, Some(variation)) => variation,
                (presentation, None) => presentation,
            },
        }
    }

    pub fn as_cell(&self) -> Cell {
        match self {
            Self::CellRef { cell, .. } => (*cell).clone(),
            Self::ClusterRef {
                text, width, attrs, ..
            } => Cell::new_grapheme_with_width(text, *width, (*attrs).clone()),
        }
    }

    pub fn same_contents(&self, other: &Self) -> bool {
        self.str() == other.str() && self.width() == other.width() && self.attrs() == other.attrs()
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.str().hash(hasher);
        self.attrs().compute_shape_hash(hasher);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloc::format;
    use siphasher::sip::SipHasher;

    fn make_cell(text: &str) -> Cell {
        Cell::new_grapheme(text, CellAttributes::default(), None)
    }

    #[allow(dead_code)]
    fn make_wide_cell(text: &str, width: usize) -> Cell {
        Cell::new_grapheme_with_width(text, width, CellAttributes::default())
    }

    #[test]
    fn cellref_cell_index() {
        let cell = make_cell("A");
        let cr = CellRef::CellRef {
            cell_index: 42,
            cell: &cell,
        };
        assert_eq!(cr.cell_index(), 42);
    }

    #[test]
    fn cellref_cluster_index() {
        let attrs = CellAttributes::default();
        let cr = CellRef::ClusterRef {
            cell_index: 7,
            text: "B",
            width: 1,
            attrs: &attrs,
        };
        assert_eq!(cr.cell_index(), 7);
    }

    #[test]
    fn cellref_str_from_cell() {
        let cell = make_cell("X");
        let cr = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        assert_eq!(cr.str(), "X");
    }

    #[test]
    fn cellref_str_from_cluster() {
        let attrs = CellAttributes::default();
        let cr = CellRef::ClusterRef {
            cell_index: 0,
            text: "hello",
            width: 5,
            attrs: &attrs,
        };
        assert_eq!(cr.str(), "hello");
    }

    #[test]
    fn cellref_width_single() {
        let cell = make_cell("a");
        let cr = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        assert_eq!(cr.width(), 1);
    }

    #[test]
    fn cellref_width_double_from_cluster() {
        let attrs = CellAttributes::default();
        let cr = CellRef::ClusterRef {
            cell_index: 0,
            text: "W",
            width: 2,
            attrs: &attrs,
        };
        assert_eq!(cr.width(), 2);
    }

    #[test]
    fn cellref_attrs_returns_default() {
        let cell = make_cell("z");
        let cr = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        assert_eq!(*cr.attrs(), CellAttributes::default());
    }

    #[test]
    fn cellref_same_contents_identical() {
        let cell1 = make_cell("A");
        let cell2 = make_cell("A");
        let cr1 = CellRef::CellRef {
            cell_index: 0,
            cell: &cell1,
        };
        let cr2 = CellRef::CellRef {
            cell_index: 5,
            cell: &cell2,
        };
        // same_contents ignores cell_index
        assert!(cr1.same_contents(&cr2));
    }

    #[test]
    fn cellref_same_contents_different_text() {
        let cell1 = make_cell("A");
        let cell2 = make_cell("B");
        let cr1 = CellRef::CellRef {
            cell_index: 0,
            cell: &cell1,
        };
        let cr2 = CellRef::CellRef {
            cell_index: 0,
            cell: &cell2,
        };
        assert!(!cr1.same_contents(&cr2));
    }

    #[test]
    fn cellref_same_contents_different_width() {
        let cell1 = make_cell("A");
        let attrs = CellAttributes::default();
        let cr1 = CellRef::CellRef {
            cell_index: 0,
            cell: &cell1,
        };
        let cr2 = CellRef::ClusterRef {
            cell_index: 0,
            text: "A",
            width: 2,
            attrs: &attrs,
        };
        assert!(!cr1.same_contents(&cr2));
    }

    #[test]
    fn cellref_as_cell_roundtrip() {
        let original = make_cell("Q");
        let cr = CellRef::CellRef {
            cell_index: 3,
            cell: &original,
        };
        let reconstructed = cr.as_cell();
        assert_eq!(reconstructed.str(), "Q");
        assert_eq!(reconstructed.width(), 1);
    }

    #[test]
    fn cellref_as_cell_from_cluster() {
        let attrs = CellAttributes::default();
        let cr = CellRef::ClusterRef {
            cell_index: 0,
            text: "W",
            width: 2,
            attrs: &attrs,
        };
        let cell = cr.as_cell();
        assert_eq!(cell.str(), "W");
        assert_eq!(cell.width(), 2);
    }

    #[test]
    fn cellref_compute_shape_hash_consistent() {
        let cell = make_cell("A");
        let cr = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        let mut h1 = SipHasher::new();
        let mut h2 = SipHasher::new();
        cr.compute_shape_hash(&mut h1);
        cr.compute_shape_hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn cellref_same_contents_cross_variant() {
        let cell = make_cell("Z");
        let attrs = CellAttributes::default();
        let cr_cell = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        let cr_cluster = CellRef::ClusterRef {
            cell_index: 0,
            text: "Z",
            width: 1,
            attrs: &attrs,
        };
        assert!(cr_cell.same_contents(&cr_cluster));
    }

    #[test]
    fn cellref_clone_copy() {
        let cell = make_cell("C");
        let cr = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        let cr2 = cr;
        assert_eq!(cr.cell_index(), cr2.cell_index());
        assert_eq!(cr.str(), cr2.str());
    }

    #[test]
    fn cellref_debug_format() {
        let cell = make_cell("D");
        let cr = CellRef::CellRef {
            cell_index: 0,
            cell: &cell,
        };
        let dbg = format!("{:?}", cr);
        assert!(dbg.contains("CellRef"));
    }
}
