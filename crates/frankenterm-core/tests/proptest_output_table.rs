//! Property-based tests for the output table module.
//!
//! Tests invariants of Alignment (Default, Copy, Clone, Debug), Column builder pattern,
//! and Table rendering properties (determinism, JSON validity, content preservation,
//! structural consistency, separator handling, strip_ansi correctness).

#![forbid(unsafe_code)]

use frankenterm_core::output::{Alignment, Column, OutputFormat, Table, strip_ansi};
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

/// Strategy that produces strings with embedded ANSI escape sequences.
fn arb_ansi_string() -> impl Strategy<Value = String> {
    (
        "[a-z]{1,8}",
        prop_oneof![
            Just("\x1b[31m"),
            Just("\x1b[32m"),
            Just("\x1b[1m"),
            Just("\x1b[0m"),
            Just("\x1b[33;1m"),
        ],
        "[a-z]{1,8}",
    )
        .prop_map(|(prefix, code, suffix)| format!("{}{}{}\x1b[0m", prefix, code, suffix))
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

// ── Alignment: Copy / Clone / Debug ─────────────────────────────────────────

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

    /// Clone produces the same Debug representation as the original.
    #[test]
    fn alignment_clone(a in arb_alignment()) {
        let cloned = Clone::clone(&a);
        let a_debug = format!("{:?}", a);
        let cloned_debug = format!("{:?}", cloned);
        prop_assert_eq!(a_debug, cloned_debug,
            "clone should produce identical Debug output");
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

    /// Debug output contains the header string.
    #[test]
    fn column_debug_contains_header(header in "[A-Za-z]{1,12}") {
        let col = Column::new(header.clone());
        let debug = format!("{:?}", col);
        prop_assert!(debug.contains(&header),
            "Debug output '{}' should contain header '{}'", debug, header);
    }

    /// Header from Column::new() is always preserved regardless of other builder calls.
    #[test]
    fn column_header_preserved(
        header in "[A-Za-z]{1,12}",
        a in arb_alignment(),
        min_w in 0usize..50,
        max_w in 0usize..100,
    ) {
        let col = Column::new(header.clone())
            .align(a)
            .min_width(min_w)
            .max_width(max_w);
        prop_assert_eq!(col.header.as_str(), header.as_str(),
            "header should be preserved through builder chain");
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

    /// len() matches the exact number of add_row calls.
    #[test]
    fn table_len_after_multiple_rows(row_count in 1usize..15) {
        let cols = vec![Column::new("X"), Column::new("Y"), Column::new("Z")];
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        for i in 0..row_count {
            table.add_row(vec![format!("a{}", i), format!("b{}", i), format!("c{}", i)]);
            prop_assert_eq!(table.len(), i + 1,
                "after {} add_row calls, len() should be {}", i + 1, i + 1);
        }
    }

    /// is_empty() returns false after adding any rows.
    #[test]
    fn table_not_empty_after_add(row_count in 1usize..8) {
        let cols = vec![Column::new("K")];
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        for i in 0..row_count {
            table.add_row(vec![format!("v{}", i)]);
        }
        prop_assert!(!table.is_empty(),
            "table with {} rows should not be empty", row_count);
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

    /// Plain render is never empty — at least has the header line.
    #[test]
    fn table_plain_render_nonempty(col_count in 1usize..5, row_count in 0usize..4) {
        let cols: Vec<Column> = (0..col_count)
            .map(|i| Column::new(format!("H{}", i)))
            .collect();
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        for i in 0..row_count {
            let cells: Vec<String> = (0..col_count).map(|j| format!("r{}c{}", i, j)).collect();
            table.add_row(cells);
        }
        let rendered = table.render();
        prop_assert!(!rendered.is_empty(),
            "plain render should never be empty");
        prop_assert!(rendered.lines().count() >= 1,
            "plain render should have at least a header line");
    }

    /// All column headers appear in the plain output.
    #[test]
    fn table_render_contains_all_headers(col_count in 1usize..5) {
        let headers: Vec<String> = (0..col_count).map(|i| format!("Hdr{}", i)).collect();
        let cols: Vec<Column> = headers.iter().map(|h| Column::new(h.clone())).collect();
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        let cells: Vec<String> = (0..col_count).map(|i| format!("val{}", i)).collect();
        table.add_row(cells);
        let rendered = table.render();
        for h in &headers {
            prop_assert!(rendered.contains(h.as_str()),
                "rendered output should contain header '{}', got: {}", h, rendered);
        }
    }
}

// ── Table: separator ────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Custom separator appears in plain render when there are multiple columns.
    #[test]
    fn table_with_separator(_i in 0..1u8) {
        let cols = vec![Column::new("Left"), Column::new("Right")];
        let mut table = Table::new(cols)
            .with_format(OutputFormat::Plain)
            .with_separator(" | ");
        table.add_row(vec!["hello".to_string(), "world".to_string()]);
        let rendered = table.render();
        prop_assert!(rendered.contains(" | "),
            "rendered output should contain custom separator ' | ', got: {}", rendered);
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

    /// JSON array length always equals the number of rows added.
    #[test]
    fn table_json_array_length_matches_rows(row_count in 0usize..10) {
        let cols = vec![Column::new("Key"), Column::new("Val")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        for i in 0..row_count {
            table.add_row(vec![format!("k{}", i), format!("v{}", i)]);
        }
        let rendered = table.render();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let arr = value.as_array().unwrap();
        prop_assert_eq!(arr.len(), row_count,
            "JSON array length should equal row count {}", row_count);
    }

    /// Every element in a JSON render is a JSON object.
    #[test]
    fn table_json_all_rows_are_objects(row_count in 1usize..8) {
        let cols = vec![Column::new("Alpha"), Column::new("Beta")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        for i in 0..row_count {
            table.add_row(vec![format!("a{}", i), format!("b{}", i)]);
        }
        let rendered = table.render();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let arr = value.as_array().unwrap();
        for (idx, elem) in arr.iter().enumerate() {
            prop_assert!(elem.is_object(),
                "JSON array element {} should be an object, got: {:?}", idx, elem);
        }
    }

    /// JSON remains valid after adding many rows.
    #[test]
    fn table_json_render_valid_after_multiple_rows(row_count in 1usize..20) {
        let cols = vec![Column::new("Seq"), Column::new("Data")];
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        for i in 0..row_count {
            table.add_row(vec![format!("{}", i), format!("payload_{}", i)]);
        }
        let rendered = table.render();
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&rendered);
        prop_assert!(parsed.is_ok(),
            "JSON render should be valid JSON after {} rows, got: {}", row_count, rendered);
        let value = parsed.unwrap();
        prop_assert!(value.is_array(), "parsed JSON should be an array");
    }
}

// ── Table: format preserves rows ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Changing output format does not lose rows (len is format-independent).
    #[test]
    fn table_with_format_preserves_rows(row_count in 1usize..8) {
        let cols_plain = vec![Column::new("A"), Column::new("B")];
        let cols_json = vec![Column::new("A"), Column::new("B")];

        let mut table_plain = Table::new(cols_plain).with_format(OutputFormat::Plain);
        let mut table_json = Table::new(cols_json).with_format(OutputFormat::Json);

        for i in 0..row_count {
            table_plain.add_row(vec![format!("x{}", i), format!("y{}", i)]);
            table_json.add_row(vec![format!("x{}", i), format!("y{}", i)]);
        }

        prop_assert_eq!(table_plain.len(), table_json.len(),
            "both tables should have the same row count");
        prop_assert_eq!(table_plain.len(), row_count,
            "row count should match number of add_row calls");

        // Both should render non-empty output
        let plain_out = table_plain.render();
        let json_out = table_json.render();
        prop_assert!(!plain_out.is_empty(), "plain render should not be empty");
        prop_assert!(!json_out.is_empty(), "json render should not be empty");
    }
}

// ── strip_ansi: correctness properties ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// strip_ansi is idempotent: applying it twice gives the same result as once.
    #[test]
    fn table_strip_ansi_idempotent(s in arb_ansi_string()) {
        let once = strip_ansi(&s);
        let twice = strip_ansi(&once);
        prop_assert_eq!(once.as_str(), twice.as_str(),
            "strip_ansi should be idempotent: strip(strip(s)) == strip(s)");
    }

    /// Plain text without any ANSI codes passes through strip_ansi unchanged.
    #[test]
    fn table_strip_ansi_plain_text_unchanged(s in "[a-zA-Z0-9 _.,!?]{1,30}") {
        let result = strip_ansi(&s);
        prop_assert_eq!(result.as_str(), s.as_str(),
            "plain text should pass through strip_ansi unchanged");
    }
}
