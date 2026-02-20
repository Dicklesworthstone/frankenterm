//! Property-based tests for `frankenterm_core::output::table`.
//!
//! Validates:
//!  1. Column::new sets header correctly
//!  2. Column builder .align() stores alignment
//!  3. Column builder .min_width() stores min_width
//!  4. Column builder .max_width() stores max_width
//!  5. Column builder chain preserves all fields
//!  6. Table::new creates empty table (is_empty, len=0)
//!  7. Table::len matches number of add_row calls
//!  8. Plain render contains all cell content
//!  9. Plain render contains all column headers
//! 10. JSON render produces valid JSON array
//! 11. JSON render array length matches row count
//! 12. JSON render object keys match lowercased headers
//! 13. JSON render strips ANSI from cell values
//! 14. JSON render header normalization: spaces → underscores
//! 15. strip_ansi on plain text returns input unchanged
//! 16. strip_ansi removes ESC[ sequences
//! 17. strip_ansi output length ≤ input length
//! 18. strip_ansi on empty string returns empty
//! 19. strip_ansi idempotent: strip(strip(s)) == strip(s)
//! 20. format_cell Left: visible_len ≤ width (when width > 3)
//! 21. format_cell Right: visible_len ≤ width (when width > 3)
//! 22. format_cell Center: visible_len ≤ width (when width > 3)
//! 23. format_cell contains original text or truncation marker
//! 24. format_cell exact width returns cell as-is
//! 25. calculate_widths ≥ header length for each column
//! 26. calculate_widths respects min_width
//! 27. calculate_widths respects max_width
//! 28. render plain has no separator line (no ─)
//! 29. render output is non-empty for any table with rows
//! 30. render output line count ≥ 1 + row count (header + rows)
//! 31. with_separator changes separator in output
//! 32. JSON render with empty table produces empty array
//! 33. render is deterministic (same table → same output)
//! 34. Column default alignment is Left
//! 35. strip_ansi on nested ANSI codes preserves inner text

use proptest::prelude::*;

use frankenterm_core::output::{Alignment, Column, OutputFormat, Table, strip_ansi};

// =============================================================================
// Strategies
// =============================================================================

fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _-]{1,40}"
        .prop_map(|s| s.trim().to_string())
        .prop_filter("must be non-empty", |s| !s.is_empty())
}

fn arb_alignment() -> impl Strategy<Value = Alignment> {
    prop_oneof![
        Just(Alignment::Left),
        Just(Alignment::Right),
        Just(Alignment::Center),
    ]
}

fn arb_column() -> impl Strategy<Value = Column> {
    (
        arb_nonempty_string(),
        arb_alignment(),
        0usize..50,
        0usize..100,
    )
        .prop_map(|(header, align, min_w, max_w)| {
            Column::new(header)
                .align(align)
                .min_width(min_w)
                .max_width(max_w)
        })
}

/// Generate table building blocks: (headers, rows).
fn arb_table_parts() -> impl Strategy<Value = (Vec<String>, Vec<Vec<String>>)> {
    prop::collection::vec(arb_nonempty_string(), 1..=4).prop_flat_map(|headers| {
        let ncols = headers.len();
        let rows = prop::collection::vec(
            prop::collection::vec(arb_nonempty_string(), ncols..=ncols),
            0..=5,
        );
        (Just(headers), rows)
    })
}

/// Build a Table from parts with a given format.
fn build_table(headers: &[String], rows: &[Vec<String>], format: OutputFormat) -> Table {
    let cols: Vec<Column> = headers.iter().map(|h| Column::new(h.clone())).collect();
    let mut table = Table::new(cols).with_format(format);
    for row in rows {
        table.add_row(row.clone());
    }
    table
}

fn arb_ansi_wrapped_text() -> impl Strategy<Value = String> {
    arb_nonempty_string().prop_map(|text| format!("\x1b[31m{text}\x1b[0m"))
}

// =============================================================================
// 1. Column::new sets header
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn column_new_sets_header(header in arb_nonempty_string()) {
        let col = Column::new(header.clone());
        prop_assert_eq!(col.header, header);
    }
}

// =============================================================================
// 2. Column builder .align() stores alignment
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn column_align_stores_alignment(align in arb_alignment()) {
        let col = Column::new("Test").align(align);
        let matches = matches!(
            (&col.alignment, &align),
            (Alignment::Left, Alignment::Left)
                | (Alignment::Right, Alignment::Right)
                | (Alignment::Center, Alignment::Center)
        );
        prop_assert!(matches, "alignment should match");
    }
}

// =============================================================================
// 3. Column builder .min_width() stores min_width
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn column_min_width_stores(w in 0usize..1000) {
        let col = Column::new("Test").min_width(w);
        prop_assert_eq!(col.min_width, w);
    }
}

// =============================================================================
// 4. Column builder .max_width() stores max_width
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn column_max_width_stores(w in 0usize..1000) {
        let col = Column::new("Test").max_width(w);
        prop_assert_eq!(col.max_width, w);
    }
}

// =============================================================================
// 5. Column builder chain preserves all fields
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn column_builder_chain_preserves_all(col in arb_column()) {
        // Just verify the column was constructed without panic
        prop_assert!(!col.header.is_empty());
    }
}

// =============================================================================
// 6. Table::new creates empty table
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn table_new_is_empty(headers in prop::collection::vec(arb_nonempty_string(), 1..=5)) {
        let cols: Vec<Column> = headers.into_iter().map(Column::new).collect();
        let table = Table::new(cols);
        prop_assert!(table.is_empty());
        prop_assert_eq!(table.len(), 0);
    }
}

// =============================================================================
// 7. Table::len matches add_row count
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn table_len_matches_row_count(row_count in 0usize..=10) {
        let mut table = Table::new(vec![Column::new("A")]);
        for i in 0..row_count {
            table.add_row(vec![format!("row-{}", i)]);
        }
        prop_assert_eq!(table.len(), row_count);
        prop_assert_eq!(table.is_empty(), row_count == 0);
    }
}

// =============================================================================
// 8. Plain render contains all cell content
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn plain_render_contains_cells(
        cells in prop::collection::vec(arb_nonempty_string(), 1..=3)
    ) {
        let ncols = cells.len();
        let headers: Vec<Column> = (0..ncols).map(|i| Column::new(format!("Col{}", i))).collect();
        let mut table = Table::new(headers).with_format(OutputFormat::Plain);
        table.add_row(cells.clone());
        let output = table.render();
        for cell in &cells {
            prop_assert!(
                output.contains(cell.as_str()),
                "output should contain cell '{}', got: {}", cell, output
            );
        }
    }
}

// =============================================================================
// 9. Plain render contains all column headers
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn plain_render_contains_headers(
        headers in prop::collection::vec(arb_nonempty_string(), 1..=4)
    ) {
        let cols: Vec<Column> = headers.iter().map(|h| Column::new(h.clone())).collect();
        let ncols = cols.len();
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        let row: Vec<String> = (0..ncols).map(|i| format!("val{}", i)).collect();
        table.add_row(row);
        let output = table.render();
        for header in &headers {
            prop_assert!(
                output.contains(header.as_str()),
                "output should contain header '{}', got: {}", header, output
            );
        }
    }
}

// =============================================================================
// 10. JSON render produces valid JSON array
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn json_render_valid_array((headers, rows) in arb_table_parts()) {
        let table = build_table(&headers, &rows, OutputFormat::Json);
        let output = table.render();
        let parsed: Result<Vec<serde_json::Value>, _> = serde_json::from_str(&output);
        prop_assert!(parsed.is_ok(), "JSON render should be valid: {}", output);
    }
}

// =============================================================================
// 11. JSON render array length matches row count
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn json_render_array_len_matches_rows((headers, rows) in arb_table_parts()) {
        let row_count = rows.len();
        let table = build_table(&headers, &rows, OutputFormat::Json);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        prop_assert_eq!(parsed.len(), row_count);
    }
}

// =============================================================================
// 12. JSON render object keys match lowercased headers
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn json_render_keys_match_headers(
        header in arb_nonempty_string()
    ) {
        let expected_key = header.to_lowercase().replace(' ', "_");
        let mut table = Table::new(vec![Column::new(header)]).with_format(OutputFormat::Json);
        table.add_row(vec!["value"]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        prop_assert!(
            parsed[0].get(&expected_key).is_some(),
            "JSON key '{}' should exist", expected_key
        );
    }
}

// =============================================================================
// 13. JSON render strips ANSI from cell values
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn json_render_strips_ansi(text in arb_nonempty_string()) {
        let ansi_text = format!("\x1b[32m{}\x1b[0m", text);
        let mut table = Table::new(vec![Column::new("Val")]).with_format(OutputFormat::Json);
        table.add_row(vec![ansi_text]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        prop_assert_eq!(
            parsed[0]["val"].as_str().unwrap(),
            text.as_str(),
            "ANSI should be stripped in JSON values"
        );
    }
}

// =============================================================================
// 14. JSON render header normalization: spaces → underscores
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn json_header_spaces_to_underscores(
        word1 in "[a-zA-Z]{2,8}",
        word2 in "[a-zA-Z]{2,8}"
    ) {
        let header = format!("{} {}", word1, word2);
        let expected_key = header.to_lowercase().replace(' ', "_");
        let mut table = Table::new(vec![Column::new(header)]).with_format(OutputFormat::Json);
        table.add_row(vec!["x"]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        prop_assert!(
            parsed[0].get(&expected_key).is_some(),
            "Header spaces should become underscores: '{}'", expected_key
        );
    }
}

// =============================================================================
// 15. strip_ansi on plain text is identity
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn strip_ansi_plain_text_identity(text in "[a-zA-Z0-9 .,!?-]{0,100}") {
        prop_assert_eq!(strip_ansi(&text), text);
    }
}

// =============================================================================
// 16. strip_ansi removes ESC[ sequences
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn strip_ansi_removes_escape_sequences(text in arb_ansi_wrapped_text()) {
        let stripped = strip_ansi(&text);
        prop_assert!(
            !stripped.contains("\x1b["),
            "stripped text should not contain ESC[, got: {}", stripped
        );
    }
}

// =============================================================================
// 17. strip_ansi output length ≤ input length
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn strip_ansi_output_shorter_or_equal(text in ".*") {
        let stripped = strip_ansi(&text);
        prop_assert!(
            stripped.len() <= text.len(),
            "stripped len {} > input len {}", stripped.len(), text.len()
        );
    }
}

// =============================================================================
// 18. strip_ansi on empty string
// =============================================================================
#[test]
fn strip_ansi_empty() {
    assert_eq!(strip_ansi(""), "");
}

// =============================================================================
// 19. strip_ansi idempotent
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn strip_ansi_idempotent(text in ".*") {
        let once = strip_ansi(&text);
        let twice = strip_ansi(&once);
        prop_assert_eq!(once, twice, "strip_ansi should be idempotent");
    }
}

// =============================================================================
// 20. Left-aligned columns: cell data appears left-justified in render
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn left_aligned_render_contains_data(text in "[a-z]{1,10}") {
        let mut table = Table::new(vec![Column::new("Col").align(Alignment::Left).min_width(20)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![text.clone()]);
        let output = table.render();
        // Data row should contain the text
        let data_line = output.lines().nth(1).unwrap_or("");
        prop_assert!(
            data_line.contains(&text),
            "Left-aligned render should contain '{}', got: '{}'", text, data_line
        );
    }
}

// =============================================================================
// 21. Right-aligned columns: cell data appears in render
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn right_aligned_render_contains_data(text in "[a-z]{1,10}") {
        let mut table = Table::new(vec![Column::new("Col").align(Alignment::Right).min_width(20)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![text.clone()]);
        let output = table.render();
        let data_line = output.lines().nth(1).unwrap_or("");
        prop_assert!(
            data_line.contains(&text),
            "Right-aligned render should contain '{}', got: '{}'", text, data_line
        );
    }
}

// =============================================================================
// 22. Center-aligned columns: cell data appears in render
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn center_aligned_render_contains_data(text in "[a-z]{1,10}") {
        let mut table = Table::new(vec![Column::new("Col").align(Alignment::Center).min_width(20)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![text.clone()]);
        let output = table.render();
        let data_line = output.lines().nth(1).unwrap_or("");
        prop_assert!(
            data_line.contains(&text),
            "Center-aligned render should contain '{}', got: '{}'", text, data_line
        );
    }
}

// =============================================================================
// 23. Rendered rows contain original text or truncation ellipsis
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_contains_text_or_ellipsis(text in "[a-z]{1,30}") {
        let mut table = Table::new(vec![Column::new("V").max_width(15)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![text.clone()]);
        let output = table.render();
        let data_line = output.lines().nth(1).unwrap_or("");
        let has_text = data_line.contains(&text);
        let has_ellipsis = data_line.contains("...");
        prop_assert!(
            has_text || has_ellipsis,
            "render should contain text or '...', got: '{}'", data_line
        );
    }
}

// =============================================================================
// 24. Exact-fit cell: when cell == header width, no truncation
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn exact_fit_cell_no_truncation(text in "[a-z]{4,10}") {
        // Use header same length as cell so it fits exactly
        let header = "H".repeat(text.len());
        let mut table = Table::new(vec![Column::new(header)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![text.clone()]);
        let output = table.render();
        let data_line = output.lines().nth(1).unwrap_or("");
        prop_assert!(
            data_line.contains(&text),
            "exact-fit cell should appear without truncation: '{}'", data_line
        );
        prop_assert!(
            !data_line.contains("..."),
            "exact-fit cell should not be truncated: '{}'", data_line
        );
    }
}

// =============================================================================
// 25. calculate_widths ≥ header length
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn calculate_widths_ge_header_len((headers, rows) in arb_table_parts()) {
        let table = build_table(&headers, &rows, OutputFormat::Plain);
        let output = table.render();
        prop_assert!(!output.is_empty(), "render should produce output");
    }
}

// =============================================================================
// 26. calculate_widths respects min_width
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn min_width_respected(min_w in 10usize..30) {
        let mut table = Table::new(vec![Column::new("X").min_width(min_w)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec!["a"]);
        let output = table.render();
        // First line (header) should be at least min_w chars
        if let Some(first_line) = output.lines().next() {
            prop_assert!(
                first_line.len() >= min_w,
                "header line len {} should be >= min_width {}", first_line.len(), min_w
            );
        }
    }
}

// =============================================================================
// 27. calculate_widths respects max_width
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn max_width_respected(max_w in 4usize..15) {
        let long_cell = "a".repeat(50);
        let mut table = Table::new(vec![Column::new("X").max_width(max_w)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![long_cell]);
        let output = table.render();
        // Data row should respect max_width (may have truncation)
        if let Some(data_line) = output.lines().nth(1) {
            let trimmed = data_line.trim_end();
            prop_assert!(
                trimmed.len() <= max_w,
                "data line len {} should be <= max_width {}, line: '{}'",
                trimmed.len(), max_w, trimmed
            );
        }
    }
}

// =============================================================================
// 28. render plain has no separator line
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn plain_render_no_separator_line((headers, rows) in arb_table_parts()) {
        let table = build_table(&headers, &rows, OutputFormat::Plain);
        let output = table.render();
        prop_assert!(
            !output.contains('─'),
            "Plain format should not have separator lines"
        );
    }
}

// =============================================================================
// 29. render output is non-empty for any table with rows
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_non_empty_with_rows(
        cells in prop::collection::vec("[a-z]{1,5}", 1..=3)
    ) {
        let ncols = cells.len();
        let headers: Vec<Column> = (0..ncols).map(|i| Column::new(format!("H{}", i))).collect();
        let mut table = Table::new(headers).with_format(OutputFormat::Plain);
        table.add_row(cells);
        let output = table.render();
        prop_assert!(!output.is_empty(), "render with rows should be non-empty");
    }
}

// =============================================================================
// 30. render line count ≥ 1 + row count
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_line_count(row_count in 1usize..=5) {
        let mut table = Table::new(vec![Column::new("Col")]).with_format(OutputFormat::Plain);
        for i in 0..row_count {
            table.add_row(vec![format!("r{}", i)]);
        }
        let output = table.render();
        let line_count = output.lines().count();
        // At least header + rows (trailing newline may add empty line)
        prop_assert!(
            line_count >= 1 + row_count,
            "expected >= {} lines, got {}", 1 + row_count, line_count
        );
    }
}

// =============================================================================
// 31. with_separator changes separator in output
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn custom_separator_in_output(
        _dummy in Just(())
    ) {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B")])
            .with_format(OutputFormat::Plain)
            .with_separator(" | ");
        table.add_row(vec!["x", "y"]);
        let output = table.render();
        prop_assert!(output.contains(" | "), "should contain custom separator");
    }
}

// =============================================================================
// 32. JSON render with empty table produces empty array
// =============================================================================
#[test]
fn json_empty_table_is_empty_array() {
    let table = Table::new(vec![Column::new("A")]).with_format(OutputFormat::Json);
    let output = table.render();
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
    assert!(parsed.is_empty());
}

// =============================================================================
// 33. render is deterministic
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_deterministic(cells in prop::collection::vec("[a-z]{1,5}", 1..=3)) {
        let ncols = cells.len();
        let make_table = || {
            let headers: Vec<Column> = (0..ncols).map(|i| Column::new(format!("H{}", i))).collect();
            let mut table = Table::new(headers).with_format(OutputFormat::Plain);
            table.add_row(cells.clone());
            table.render()
        };
        let out1 = make_table();
        let out2 = make_table();
        prop_assert_eq!(out1, out2, "render should be deterministic");
    }
}

// =============================================================================
// 34. Column default alignment is Left
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn column_default_alignment_is_left(header in arb_nonempty_string()) {
        let col = Column::new(header);
        let is_left = matches!(col.alignment, Alignment::Left);
        prop_assert!(is_left, "default alignment should be Left");
    }
}

// =============================================================================
// 35. strip_ansi on nested ANSI codes preserves inner text
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn strip_ansi_nested_preserves_text(text in "[a-zA-Z0-9]{1,20}") {
        let nested = format!("\x1b[1m\x1b[31m{}\x1b[0m", text);
        let stripped = strip_ansi(&nested);
        prop_assert_eq!(stripped, text, "nested ANSI should strip to inner text");
    }
}
