use crate::{Arc, HashMap};
use frankenterm_cell::MAX_CUSTOM_CELL_WIDTH_EXPANSION;
use frankenterm_dynamic::{FromDynamic, ToDynamic};
use std::convert::TryFrom;

#[derive(Clone, Debug, Eq, PartialEq, FromDynamic, ToDynamic)]
pub struct CellWidth {
    pub first: u32,
    pub last: u32,
    pub width: u8,
}

fn validate_cell_width_slice(cell_widths: &[CellWidth]) -> Result<usize, String> {
    let mut expanded_entries = 0u64;
    let maximum_expanded_entries = u64::try_from(MAX_CUSTOM_CELL_WIDTH_EXPANSION)
        .map_err(|_| "cell-width expansion cap does not fit u64".to_string())?;
    for (index, cell_width) in cell_widths.iter().enumerate() {
        if cell_width.first > cell_width.last {
            return Err(format!(
                "cell_widths[{index}] has a descending codepoint range"
            ));
        }
        if !(1..=2).contains(&cell_width.width) {
            return Err(format!(
                "cell_widths[{index}].width must be one or two columns"
            ));
        }
        if cell_width.last > u32::from(char::MAX) {
            return Err(format!(
                "cell_widths[{index}] exceeds the maximum Unicode codepoint"
            ));
        }
        if cell_width.first <= 0xdfff && cell_width.last >= 0xd800 {
            return Err(format!(
                "cell_widths[{index}] intersects the Unicode surrogate range"
            ));
        }
        let range_entries = u64::from(cell_width.last)
            .checked_sub(u64::from(cell_width.first))
            .and_then(|span| span.checked_add(1))
            .ok_or_else(|| format!("cell_widths[{index}] range size overflowed"))?;
        expanded_entries = expanded_entries
            .checked_add(range_entries)
            .ok_or_else(|| "cell_widths expanded entry count overflowed".to_string())?;
        if expanded_entries > maximum_expanded_entries {
            return Err(format!(
                "cell_widths expands to more than {MAX_CUSTOM_CELL_WIDTH_EXPANSION} entries"
            ));
        }
    }
    usize::try_from(expanded_entries)
        .map_err(|_| "cell_widths expanded entry count does not fit usize".to_string())
}

pub(crate) fn validate_cell_widths(value: &Option<Vec<CellWidth>>) -> Result<(), String> {
    if let Some(cell_widths) = value {
        validate_cell_width_slice(cell_widths)?;
    }
    Ok(())
}

impl CellWidth {
    pub(crate) fn compile_to_map(
        cellwidths: Option<Vec<Self>>,
    ) -> Option<Arc<HashMap<u32, u8>>> {
        let cellwidths = cellwidths.as_ref()?;
        let expanded_entries = match validate_cell_width_slice(cellwidths) {
            Ok(expanded_entries) => expanded_entries,
            Err(error) => {
                log::error!("refusing invalid custom cell-width map: {error}");
                return None;
            }
        };
        let mut map = HashMap::new();
        if map.try_reserve(expanded_entries).is_err() {
            log::error!("custom cell-width map allocation failed");
            return None;
        }
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
    fn validation_rejects_invalid_or_unbounded_ranges() {
        for invalid in [
            CellWidth {
                first: 2,
                last: 1,
                width: 1,
            },
            CellWidth {
                first: 1,
                last: 1,
                width: 0,
            },
            CellWidth {
                first: 0xd7ff,
                last: 0xd800,
                width: 1,
            },
            CellWidth {
                first: 0x10ffff,
                last: 0x110000,
                width: 1,
            },
            CellWidth {
                first: 0x10000,
                last: 0x50000,
                width: 1,
            },
        ] {
            let input = Some(vec![invalid]);
            assert!(validate_cell_widths(&input).is_err());
            assert!(CellWidth::compile_to_map(input).is_none());
        }
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
