use frankenterm_core::storage_backend_cells::{Row, RowCells, SqlCell};
use proptest::prelude::*;

fn text_cell() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.,:/?&=-]{0,64}".prop_map(String::from)
}

fn blob_cell() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..64)
}

fn finite_f64() -> impl Strategy<Value = f64> {
    any::<f64>().prop_filter("finite f64", |value| value.is_finite())
}

fn sql_cell() -> impl Strategy<Value = SqlCell> {
    prop_oneof![
        Just(SqlCell::Null),
        any::<i64>().prop_map(SqlCell::Integer),
        finite_f64().prop_map(SqlCell::Real),
        text_cell().prop_map(SqlCell::Text),
        blob_cell().prop_map(SqlCell::Blob),
    ]
}

fn sql_cells() -> impl Strategy<Value = Vec<SqlCell>> {
    prop::collection::vec(sql_cell(), 0..24)
}

fn prop_assert_cell_semantically_eq(left: &SqlCell, right: &SqlCell) -> Result<(), TestCaseError> {
    match (left, right) {
        (SqlCell::Real(left), SqlCell::Real(right)) => {
            if left.to_bits() == right.to_bits() {
                return Ok(());
            }
            let diff = (left - right).abs();
            let scale = left.abs().max(right.abs()).max(f64::MIN_POSITIVE);
            prop_assert!(diff <= scale * f64::EPSILON);
            Ok(())
        }
        _ => {
            prop_assert_eq!(left, right);
            Ok(())
        }
    }
}

fn prop_assert_row_cells_semantically_eq(
    left: &RowCells,
    right: &RowCells,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(left.cells.len(), right.cells.len());
    for (left, right) in left.cells.iter().zip(&right.cells) {
        prop_assert_cell_semantically_eq(left, right)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_storage_backend_cells_variant_accessors_are_exact(cell in sql_cell()) {
        prop_assert_eq!(cell.is_null(), matches!(cell, SqlCell::Null));
        prop_assert_eq!(cell.as_i64().is_some(), matches!(cell, SqlCell::Integer(_)));
        prop_assert_eq!(cell.as_f64().is_some(), matches!(cell, SqlCell::Real(_)));
        prop_assert_eq!(cell.as_text().is_some(), matches!(cell, SqlCell::Text(_)));
        prop_assert_eq!(cell.as_blob().is_some(), matches!(cell, SqlCell::Blob(_)));

        match &cell {
            SqlCell::Null => {
                prop_assert_eq!(cell.as_i64(), None);
                prop_assert_eq!(cell.as_f64(), None);
                prop_assert_eq!(cell.as_text(), None);
                prop_assert_eq!(cell.as_blob(), None);
            }
            SqlCell::Integer(value) => prop_assert_eq!(cell.as_i64(), Some(*value)),
            SqlCell::Real(value) => prop_assert_eq!(cell.as_f64(), Some(*value)),
            SqlCell::Text(value) => prop_assert_eq!(cell.as_text(), Some(value.as_str())),
            SqlCell::Blob(value) => prop_assert_eq!(cell.as_blob(), Some(value.as_slice())),
        }
    }

    #[test]
    fn proptest_storage_backend_cells_row_accessors_match_underlying_cells(
        cells in sql_cells(),
        extra_idx in 0_usize..=8,
    ) {
        let row = RowCells::new(cells.clone());

        prop_assert_eq!(row.cell_count(), cells.len());
        for (idx, cell) in cells.iter().enumerate() {
            prop_assert_eq!(row.cell(idx), Some(cell));
            prop_assert_eq!(row.is_null(idx), cell.is_null());
            prop_assert_eq!(row.get_i64(idx), cell.as_i64());
            prop_assert_eq!(row.get_f64(idx), cell.as_f64());
            prop_assert_eq!(row.get_text(idx), cell.as_text());
            prop_assert_eq!(row.get_blob(idx), cell.as_blob());
        }

        let out_of_bounds = cells.len() + extra_idx;
        prop_assert_eq!(row.cell(out_of_bounds), None);
        prop_assert!(!row.is_null(out_of_bounds));
        prop_assert_eq!(row.get_i64(out_of_bounds), None);
        prop_assert_eq!(row.get_f64(out_of_bounds), None);
        prop_assert_eq!(row.get_text(out_of_bounds), None);
        prop_assert_eq!(row.get_blob(out_of_bounds), None);
    }

    #[test]
    fn proptest_storage_backend_cells_from_vec_and_serde_roundtrip(cells in sql_cells()) {
        let row = RowCells::from(cells.clone());
        prop_assert_eq!(&row.cells, &cells);

        let json = serde_json::to_string(&row).expect("serialize row cells");
        let parsed: RowCells = serde_json::from_str(&json).expect("deserialize row cells");
        prop_assert_row_cells_semantically_eq(&parsed, &row)?;
    }

    #[test]
    fn proptest_storage_backend_cells_canonical_string_mapping(
        text in text_cell(),
        nonempty in "[A-Za-z0-9 _.,:/?&=-]{1,64}",
    ) {
        prop_assert!(SqlCell::from_canonical_string("").is_null());

        let from_text = SqlCell::from_canonical_string(&text);
        if text.is_empty() {
            prop_assert!(from_text.is_null());
        } else {
            prop_assert_eq!(from_text.as_text(), Some(text.as_str()));
        }

        let from_nonempty = SqlCell::from_canonical_string(&nonempty);
        if let Ok(i) = nonempty.parse::<i64>() {
            if i.to_string() == nonempty {
                prop_assert_eq!(from_nonempty, SqlCell::Integer(i));
                return Ok(());
            }
        }
        if let Ok(f) = nonempty.parse::<f64>() {
            if f.to_string() == nonempty {
                prop_assert_eq!(from_nonempty, SqlCell::Real(f));
                return Ok(());
            }
        }
        prop_assert_eq!(from_nonempty.as_text(), Some(nonempty.as_str()));
        prop_assert!(!from_nonempty.is_null());
    }

    #[test]
    fn proptest_storage_backend_cells_dyn_row_dispatch_matches_concrete(cells in sql_cells()) {
        let concrete = RowCells::new(cells.clone());
        let boxed: Box<dyn Row> = Box::new(RowCells::new(cells));

        prop_assert_eq!(boxed.cell_count(), concrete.cell_count());
        for idx in 0..=concrete.cell_count() {
            prop_assert_eq!(boxed.cell(idx), concrete.cell(idx));
            prop_assert_eq!(boxed.is_null(idx), concrete.is_null(idx));
            prop_assert_eq!(boxed.get_i64(idx), concrete.get_i64(idx));
            prop_assert_eq!(boxed.get_f64(idx), concrete.get_f64(idx));
            prop_assert_eq!(boxed.get_text(idx), concrete.get_text(idx));
            prop_assert_eq!(boxed.get_blob(idx), concrete.get_blob(idx));
        }
    }
}
