use frankenterm_core::storage_backend_helpers::{
    count_table_where, execute_typed, row_exists_where,
};
use frankenterm_core::storage_backend_row_helpers::{
    RowReader, row_blob_size, row_bool, row_f64, row_i64, row_i64_or, row_optional_string,
    row_string, row_u32,
};
use frankenterm_core::storage_backend_trait::{
    OpenConfig, RusqliteBackend, StorageBackend, ToSqlValue,
};
use proptest::prelude::*;

fn row(cells: Vec<String>) -> Vec<String> {
    cells
}

fn text_cell() -> impl Strategy<Value = String> {
    "[A-Za-z0-9 _.,:/?&=-]{1,64}".prop_map(String::from)
}

fn invalid_bool_cell() -> impl Strategy<Value = String> {
    "[A-Za-z2-9_ -]{1,32}"
        .prop_map(String::from)
        .prop_filter("not a supported bool encoding", |s| {
            !matches!(s.as_str(), "0" | "1" | "true" | "false")
        })
}

fn backend_with_flags(rows: &[(String, bool)]) -> RusqliteBackend {
    let backend = RusqliteBackend::open(":memory:", &OpenConfig::default()).unwrap();
    backend
        .execute_batch("CREATE TABLE flags (id INTEGER PRIMARY KEY, name TEXT, active INTEGER);")
        .unwrap();

    for (idx, (name, active)) in rows.iter().enumerate() {
        execute_typed(
            &backend,
            "INSERT INTO flags (id, name, active) VALUES (?1, ?2, ?3)",
            &[
                ToSqlValue::Integer(i64::try_from(idx + 1).expect("test id fits i64")),
                ToSqlValue::Text(name.as_str()),
                ToSqlValue::bool(*active),
            ],
        )
        .unwrap();
    }

    backend
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_storage_backend_row_helpers_i64_and_optional_i64_roundtrip(
        prefix in prop::collection::vec(text_cell(), 0..4),
        value in any::<i64>(),
        default in any::<i64>(),
    ) {
        let idx = prefix.len();
        let mut cells = prefix;
        cells.push(value.to_string());
        cells.push(String::new());
        let row = row(cells);
        let reader = RowReader::new(&row);

        prop_assert_eq!(row_i64(&row, idx).expect("row_i64 parses generated i64"), value);
        prop_assert_eq!(reader.i64(idx).expect("reader parses generated i64"), value);
        prop_assert_eq!(
            row_i64_or(&row, idx + 1, default).expect("empty i64 cell uses default"),
            default
        );
        prop_assert_eq!(
            reader
                .optional_i64(idx)
                .expect("reader parses generated optional i64"),
            Some(value)
        );
        prop_assert_eq!(
            reader
                .optional_i64(idx + 1)
                .expect("empty optional i64 cell maps to none"),
            None
        );
        prop_assert_eq!(reader.column_count(), idx + 2);
    }

    #[test]
    fn proptest_storage_backend_row_helpers_u32_accepts_only_u32_domain(
        value in any::<u32>(),
        negative in 1_i64..=i64::MAX,
        too_large_delta in 1_u64..=1024,
    ) {
        let valid = row(vec![value.to_string()]);
        let negative_row = row(vec![format!("-{negative}")]);
        let too_large = u64::from(u32::MAX) + too_large_delta;
        let too_large_row = row(vec![too_large.to_string()]);

        prop_assert_eq!(row_u32(&valid, 0).expect("generated u32 parses"), value);
        prop_assert!(row_u32(&negative_row, 0).is_err());
        prop_assert!(row_u32(&too_large_row, 0).is_err());
    }

    #[test]
    fn proptest_storage_backend_row_helpers_f64_roundtrips_finite_values(
        value in any::<f64>(),
    ) {
        prop_assume!(value.is_finite());
        let encoded = value.to_string();
        let row = row(vec![encoded]);
        let parsed = row_f64(&row, 0).expect("finite generated f64 parses");

        if value == 0.0 {
            prop_assert_eq!(parsed, value);
        } else {
            prop_assert_eq!(parsed.to_bits(), value.to_bits());
        }
    }

    #[test]
    fn proptest_storage_backend_row_helpers_string_optional_and_bool_semantics(
        text in text_cell(),
        truthy in prop_oneof![Just("1".to_string()), Just("true".to_string())],
        falsy in prop_oneof![Just("0".to_string()), Just("false".to_string())],
        other in invalid_bool_cell(),
    ) {
        let row = row(vec![
            text.clone(),
            String::new(),
            truthy,
            falsy,
            other,
        ]);
        let reader = RowReader::new(&row);

        prop_assert_eq!(
            row_string(&row, 0).expect("string helper returns text"),
            text.clone()
        );
        prop_assert_eq!(
            row_optional_string(&row, 0).expect("optional string returns text"),
            Some(text.clone())
        );
        prop_assert_eq!(
            reader
                .optional_string(1)
                .expect("empty optional string maps to none"),
            None
        );
        prop_assert!(reader.bool(2).expect("truthy bool parses"));
        prop_assert!(!row_bool(&row, 3).expect("falsy bool parses"));
        prop_assert!(row_bool(&row, 4).is_err());
    }

    #[test]
    fn proptest_storage_backend_row_helpers_blob_size_and_bounds_checks(
        leading in prop::collection::vec(text_cell(), 0..4),
        blob_size in 0_usize..=1_000_000,
        extra_idx in 0_usize..=8,
    ) {
        let idx = leading.len();
        let mut cells = leading;
        cells.push(format!("<blob:{blob_size} bytes>"));
        cells.push(format!("<blob:{blob_size}>"));
        let row = row(cells);
        let reader = RowReader::new(&row);

        prop_assert_eq!(
            row_blob_size(&row, idx).expect("generated blob size parses"),
            blob_size
        );
        prop_assert_eq!(
            reader.blob_size(idx).expect("reader parses blob size"),
            blob_size
        );
        prop_assert!(row_blob_size(&row, idx + 1).is_err());
        prop_assert!(row_i64(&row, row.len() + extra_idx).is_err());
    }

    #[test]
    fn proptest_storage_backend_helpers_typed_filters_match_manual_count(
        rows in prop::collection::vec((text_cell(), any::<bool>()), 0..32),
        needle in text_cell(),
        active in any::<bool>(),
    ) {
        let backend = backend_with_flags(&rows);
        let expected = rows
            .iter()
            .filter(|(name, row_active)| name == &needle && *row_active == active)
            .count();

        let observed = count_table_where(
            &backend,
            "flags",
            "name = ?1 AND active = ?2",
            &[ToSqlValue::Text(needle.as_str()), ToSqlValue::bool(active)],
        )
        .expect("typed filtered count succeeds");
        let exists = row_exists_where(
            &backend,
            "flags",
            "name = ?1 AND active = ?2",
            &[ToSqlValue::Text(needle.as_str()), ToSqlValue::bool(active)],
        )
        .expect("typed filtered existence succeeds");

        prop_assert_eq!(observed, i64::try_from(expected).expect("expected count fits i64"));
        prop_assert_eq!(exists, expected > 0);
    }

    #[test]
    fn proptest_storage_backend_helpers_threshold_count_and_exists_are_consistent(
        rows in prop::collection::vec((text_cell(), any::<bool>()), 0..32),
        threshold in 0_i64..40,
    ) {
        let backend = backend_with_flags(&rows);
        let expected = rows
            .iter()
            .enumerate()
            .filter(|(idx, _)| i64::try_from(idx + 1).expect("test id fits i64") >= threshold)
            .count();

        let observed = count_table_where(
            &backend,
            "flags",
            "id >= ?1",
            &[ToSqlValue::Integer(threshold)],
        )
        .expect("typed threshold count succeeds");
        let exists = row_exists_where(
            &backend,
            "flags",
            "id >= ?1",
            &[ToSqlValue::Integer(threshold)],
        )
        .expect("typed threshold existence succeeds");

        prop_assert_eq!(observed, i64::try_from(expected).expect("expected count fits i64"));
        prop_assert_eq!(exists, expected > 0);
    }
}
