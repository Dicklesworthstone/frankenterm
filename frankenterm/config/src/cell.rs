use crate::{Arc, HashMap};
use frankenterm_dynamic::{FromDynamic, ToDynamic};

#[derive(Clone, Debug, Eq, PartialEq, FromDynamic, ToDynamic)]
pub struct CellWidth {
    pub first: u32,
    pub last: u32,
    pub width: u8,
}

impl CellWidth {
    pub fn compile_to_map(cellwidths: Option<Vec<Self>>) -> Option<Arc<HashMap<u32, u8>>> {
        let cellwidths = cellwidths.as_ref()?;
        let mut map = HashMap::new();
        for cellwidth in cellwidths {
            for i in cellwidth.first..=cellwidth.last {
                map.insert(i, cellwidth.width);
            }
        }
        Some(map.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_to_map_returns_none_when_input_missing() {
        assert!(CellWidth::compile_to_map(None).is_none());
    }

    #[test]
    fn compile_to_map_expands_ranges_and_overwrites_on_overlap() {
        let map = CellWidth::compile_to_map(Some(vec![
            CellWidth {
                first: 10,
                last: 12,
                width: 1,
            },
            CellWidth {
                first: 12,
                last: 13,
                width: 2,
            },
        ]))
        .expect("map");

        assert_eq!(map.get(&10), Some(&1));
        assert_eq!(map.get(&11), Some(&1));
        assert_eq!(map.get(&12), Some(&2));
        assert_eq!(map.get(&13), Some(&2));
        assert_eq!(map.get(&9), None);
    }

    #[test]
    fn compile_to_map_empty_vec() {
        let result = CellWidth::compile_to_map(Some(vec![]));
        let map = result.unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn compile_to_map_single_codepoint() {
        let cw = CellWidth {
            first: 0x3000,
            last: 0x3000,
            width: 2,
        };
        let map = CellWidth::compile_to_map(Some(vec![cw])).unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map.get(&0x3000), Some(&2));
    }

    #[test]
    fn compile_to_map_disjoint_ranges() {
        let entries = vec![
            CellWidth {
                first: 1,
                last: 3,
                width: 1,
            },
            CellWidth {
                first: 100,
                last: 102,
                width: 2,
            },
        ];
        let map = CellWidth::compile_to_map(Some(entries)).unwrap();
        assert_eq!(map.len(), 6);
        assert_eq!(map.get(&2), Some(&1));
        assert_eq!(map.get(&101), Some(&2));
        assert_eq!(map.get(&50), None);
    }

    #[test]
    fn cellwidth_equality() {
        let a = CellWidth {
            first: 1,
            last: 5,
            width: 2,
        };
        let b = CellWidth {
            first: 1,
            last: 5,
            width: 2,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn cellwidth_inequality() {
        let a = CellWidth {
            first: 1,
            last: 5,
            width: 2,
        };
        let b = CellWidth {
            first: 1,
            last: 5,
            width: 1,
        };
        assert_ne!(a, b);
    }
}
