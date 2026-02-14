//! Property-based tests for the output table module.
//!
//! Tests invariants of Alignment (Default, Copy, Debug), Column builder pattern,
//! and Table rendering properties (determinism, JSON validity, content preservation,
//! structural consistency).

use frankenterm_core::output::{Alignment, Column, OutputFormat, Table};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_alignment() -> impl Strategy<Value = Alignment> {
    prop_oneof![
        Just(Alignment::Left),
        Just(Alignment::Right),
        Just(Alignment::Center),
    ]
}

fn arb_column() -> impl Strategy<Value = Column> {
    (
        "[A-Za-z _]{1,15}", // header
        arb_alignment(),
        0usize..20, // min_width
        0usize..50, // max_width
    )
        .prop_map(|(header, alignment, min_width, max_width)| {
            Column::new(header)
                .align(alignment)
                .min_width(min_width)
                .max_width(max_width)
        })
}

fn arb_output_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![Just(OutputFormat::Plain), Just(OutputFormat::Json),]
}

// ── Alignment: Default ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default alignment is Left.
    #[test]
    fn alignment_default_is_left(_i in 0..1u8) {
        let d = Alignment::default();
        let debug = format!("{:?}", d);
        prop_assert!(debug.contains("Left"), "default should be Left, got {}", debug);
    }

    /// All three variants are distinct.
    #[test]
    fn alignment_variants_distinct(_i in 0..1u8) {
        let left = format!("{:?}", Alignment::Left);
        let right = format!("{:?}", Alignment::Right);
        let center = format!("{:?}", Alignment::Center);
        prop_assert_ne!(left.as_str(), right.as_str());
        prop_assert_ne!(left.as_str(), center.as_str());
        prop_assert_ne!(right.as_str(), center.as_str());
    }
}

// ── Alignment: Copy / Debug ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Copy semantics work.
    #[test]
    fn alignment_copy(a in arb_alignment()) {
        let copied = a;
        let a_debug = format!("{:?}", a);
        let copied_debug = format!("{:?}", copied);
        prop_assert_eq!(a_debug.as_str(), copied_debug.as_str());
    }

    /// Debug format is non-empty.
    #[test]
    fn alignment_debug_non_empty(a in arb_alignment()) {
        let debug = format!("{:?}", a);
        prop_assert!(!debug.is_empty());
    }
}

// ── Column: builder pattern ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Column::new sets header and default alignment.
    #[test]
    fn column_new_sets_header(header in "[A-Za-z]{1,15}") {
        let col = Column::new(header.clone());
        prop_assert_eq!(col.header.as_str(), header.as_str());
        let debug = format!("{:?}", col.alignment);
        prop_assert!(debug.contains("Left"), "default alignment should be Left");
        prop_assert_eq!(col.min_width, 0);
        prop_assert_eq!(col.max_width, 0);
    }

    /// align() sets the alignment.
    #[test]
    fn column_align(a in arb_alignment()) {
        let col = Column::new("test").align(a);
        let col_debug = format!("{:?}", col.alignment);
        let expected_debug = format!("{:?}", a);
        prop_assert_eq!(col_debug.as_str(), expected_debug.as_str());
    }

    /// min_width() sets the minimum width.
    #[test]
    fn column_min_width(w in 0usize..100) {
        let col = Column::new("test").min_width(w);
        prop_assert_eq!(col.min_width, w);
    }

    /// max_width() sets the maximum width.
    #[test]
    fn column_max_width(w in 0usize..100) {
        let col = Column::new("test").max_width(w);
        prop_assert_eq!(col.max_width, w);
    }

    /// Builder methods are chainable and independent.
    #[test]
    fn column_builder_chain(
        header in "[A-Za-z]{1,10}",
        a in arb_alignment(),
        min_w in 0usize..50,
        max_w in 0usize..100,
    ) {
        let col = Column::new(header.clone())
            .align(a)
            .min_width(min_w)
            .max_width(max_w);
        prop_assert_eq!(col.header.as_str(), header.as_str());
        prop_assert_eq!(col.min_width, min_w);
        prop_assert_eq!(col.max_width, max_w);
    }

    /// Clone produces equivalent column.
    #[test]
    fn column_clone(col in arb_column()) {
        let cloned = col.clone();
        prop_assert_eq!(cloned.header.as_str(), col.header.as_str());
        prop_assert_eq!(cloned.min_width, col.min_width);
        prop_assert_eq!(cloned.max_width, col.max_width);
    }

    /// Debug format is non-empty.
    #[test]
    fn column_debug_non_empty(col in arb_column()) {
        let debug = format!("{:?}", col);
        prop_assert!(!debug.is_empty());
    }
}

// ── Table: empty table ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Empty table is_empty and len() == 0.
    #[test]
    fn table_empty_invariants(col_count in 1usize..5) {
        let cols: Vec<Column> = (0..col_count)
            .map(|i| Column::new(format!("Col{}", i)))
            .collect();
        let table = Table::new(cols).with_format(OutputFormat::Plain);
        prop_assert!(table.is_empty(), "new table should be empty");
        prop_assert_eq!(table.len(), 0, "new table len should be 0");
    }

    /// Empty table render still contains headers.
    #[test]
    fn table_empty_render_has_headers(col_count in 1usize..4) {
        let headers: Vec<String> = (0..col_count).map(|i| format!("Header{}", i)).collect();
        let cols: Vec<Column> = headers.iter().map(|h| Column::new(h.clone())).collect();
        let table = Table::new(cols).with_format(OutputFormat::Plain);
        let rendered = table.render();
        for h in &headers {
            prop_assert!(rendered.contains(h.as_str()),
                "rendered output should contain header '{}', got: {}", h, rendered);
        }
    }
}

// ── Table: row operations ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Adding rows increments len and clears is_empty.
    #[test]
    fn table_add_row_updates_len(row_count in 1usize..10) {
        let cols = vec![Column::new("A"), Column::new("B")];
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        for i in 0..row_count {
            table.add_row(vec![format!("r{}", i), format!("v{}", i)]);
        }
        prop_assert_eq!(table.len(), row_count);
        prop_assert!(!table.is_empty());
    }
}

// ── Table: render determinism ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Rendering the same table twice produces identical output.
    #[test]
    fn table_render_deterministic(
        format in arb_output_format(),
        row_count in 0usize..5,
    ) {
        let cols = vec![Column::new("Name"), Column::new("Value")];
        let mut table = Table::new(cols).with_format(format);
        for i in 0..row_count {
            table.add_row(vec![format!("key{}", i), format!("val{}", i)]);
        }
        let r1 = table.render();
        let r2 = table.render();
        prop_assert_eq!(r1.as_str(), r2.as_str(), "render should be deterministic");
    }
}

// ── Table: Plain render contains all cell data ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Plain render contains every cell's content.
    #[test]
    fn table_plain_contains_cells(
        cell1 in "[a-z]{1,10}",
        cell2 in "[a-z]{1,10}",
    ) {
        let cols = vec![Column::new("A"), Column::new("B")];
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        table.add_row(vec![cell1.clone(), cell2.clone()]);
        let rendered = table.render();
        prop_assert!(rendered.contains(cell1.as_str()),
            "rendered should contain cell1 '{}', got: {}", cell1, rendered);
        prop_assert!(rendered.contains(cell2.as_str()),
            "rendered should contain cell2 '{}', got: {}", cell2, rendered);
    }

    /// Plain render has exactly (1 header + N data rows) line count.
    #[test]
    fn table_plain_line_count(row_count in 0usize..8) {
        let cols = vec![Column::new("X")];
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        for i in 0..row_count {
            table.add_row(vec![format!("r{}", i)]);
        }
        let rendered = table.render();
        // Plain: header line + data rows, no separator line
        let lines: Vec<&str> = rendered.lines().collect();
        prop_assert_eq!(lines.len(), 1 + row_count,
            "expected {} lines (1 header + {} rows), got {}: {:?}",
            1 + row_count, row_count, lines.len(), lines);
    }
}

// ── Table: JSON render ──────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// JSON render produces valid JSON array.
    #[test]
    fn table_json_valid(row_count in 0usize..5) {
        let cols = vec![Column::new("ID"), Column::new("Name")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        for i in 0..row_count {
            table.add_row(vec![format!("{}", i), format!("name{}", i)]);
        }
        let rendered = table.render();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        prop_assert!(value.is_array(),
            "JSON render should produce an array");
        let arr = value.as_array().unwrap();
        prop_assert_eq!(arr.len(), row_count,
            "JSON array length should match row count");
    }

    /// JSON render uses lowercase headers as keys.
    #[test]
    fn table_json_lowercase_keys(
        cell in "[a-z]{1,10}",
    ) {
        let cols = vec![Column::new("MyHeader")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        table.add_row(vec![cell.clone()]);
        let rendered = table.render();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let arr = value.as_array().unwrap();
        let obj = arr[0].as_object().unwrap();
        prop_assert!(obj.contains_key("myheader"),
            "JSON key should be lowercase 'myheader', got keys: {:?}", obj.keys().collect::<Vec<_>>());
    }

    /// JSON render replaces spaces in headers with underscores.
    #[test]
    fn table_json_space_to_underscore(
        cell in "[a-z]{1,10}",
    ) {
        let cols = vec![Column::new("My Header")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        table.add_row(vec![cell.clone()]);
        let rendered = table.render();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let arr = value.as_array().unwrap();
        let obj = arr[0].as_object().unwrap();
        prop_assert!(obj.contains_key("my_header"),
            "JSON key should replace spaces with underscores");
    }

    /// JSON render preserves cell values.
    #[test]
    fn table_json_preserves_cells(
        cell in "[a-z0-9]{1,15}",
    ) {
        let cols = vec![Column::new("Val")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        table.add_row(vec![cell.clone()]);
        let rendered = table.render();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let arr = value.as_array().unwrap();
        let val = arr[0].get("val").unwrap().as_str().unwrap();
        prop_assert_eq!(val, cell.as_str(),
            "JSON should preserve cell value");
    }
}
