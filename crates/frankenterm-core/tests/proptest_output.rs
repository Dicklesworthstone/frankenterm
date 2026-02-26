//! Property-based tests for the output module
//!
//! Tests: OutputFormat parsing/display, Style conditional formatting,
//! strip_ansi idempotency, Table rendering invariants, Column builder properties.
//!
//! Coverage: 30 property-based tests

use frankenterm_core::output::{
    Alignment, Column, EffectiveFormat, OutputFormat, Style, Table, strip_ansi,
};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_output_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![
        Just(OutputFormat::Auto),
        Just(OutputFormat::Plain),
        Just(OutputFormat::Json),
    ]
}

fn arb_alignment() -> impl Strategy<Value = Alignment> {
    prop_oneof![
        Just(Alignment::Left),
        Just(Alignment::Right),
        Just(Alignment::Center),
    ]
}

/// Arbitrary printable ASCII text (no control chars except spaces)
fn arb_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.,!?-]{0,80}"
}

/// Arbitrary short header text (non-empty, no whitespace for clean JSON keys)
fn arb_header() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9]{0,15}"
}

/// Generate a string with embedded ANSI codes
fn arb_ansi_text() -> impl Strategy<Value = String> {
    (arb_text(), prop::collection::vec(arb_text(), 0..3)).prop_map(|(base, extras)| {
        if extras.is_empty() {
            base
        } else {
            let mut s = String::new();
            s.push_str("\x1b[31m");
            s.push_str(&base);
            s.push_str("\x1b[0m");
            for e in &extras {
                s.push_str("\x1b[1m");
                s.push_str(e);
                s.push_str("\x1b[0m");
            }
            s
        }
    })
}

// =============================================================================
// OutputFormat properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 1. OutputFormat display→parse roundtrip
    #[test]
    fn output_format_display_parse_roundtrip(fmt in arb_output_format()) {
        let displayed = fmt.to_string();
        let parsed = OutputFormat::parse(&displayed);
        prop_assert_eq!(parsed, Some(fmt));
    }

    // 2. OutputFormat parse is case-insensitive
    #[test]
    fn output_format_parse_case_insensitive(fmt in arb_output_format()) {
        let displayed = fmt.to_string();
        let upper = displayed.to_uppercase();
        let mixed = {
            let mut chars: Vec<char> = displayed.chars().collect();
            if !chars.is_empty() {
                chars[0] = chars[0].to_uppercase().next().unwrap_or(chars[0]);
            }
            chars.into_iter().collect::<String>()
        };
        prop_assert_eq!(OutputFormat::parse(&upper), Some(fmt));
        prop_assert_eq!(OutputFormat::parse(&mixed), Some(fmt));
    }

    // 3. OutputFormat::Json always reports is_json=true
    #[test]
    fn json_always_reports_is_json(_seed in 0u32..1000) {
        prop_assert!(OutputFormat::Json.is_json());
        prop_assert!(!OutputFormat::Json.is_rich());
        prop_assert!(!OutputFormat::Plain.is_json());
        prop_assert!(!OutputFormat::Auto.is_json());
    }

    // 4. OutputFormat::Plain always reports is_plain=true, is_rich=false
    #[test]
    fn plain_format_properties(_seed in 0u32..1000) {
        prop_assert!(OutputFormat::Plain.is_plain());
        prop_assert!(!OutputFormat::Plain.is_rich());
        prop_assert!(!OutputFormat::Plain.is_json());
    }

    // 5. OutputFormat effective() for non-Auto is deterministic
    #[test]
    fn effective_format_non_auto_deterministic(fmt in prop_oneof![Just(OutputFormat::Plain), Just(OutputFormat::Json)]) {
        let eff1 = fmt.effective();
        let eff2 = fmt.effective();
        prop_assert_eq!(eff1, eff2);
        match fmt {
            OutputFormat::Plain => prop_assert_eq!(eff1, EffectiveFormat::Plain),
            OutputFormat::Json => prop_assert_eq!(eff1, EffectiveFormat::Json),
            OutputFormat::Auto => {}
        }
    }

    // 6. Invalid format strings return None
    #[test]
    fn invalid_format_strings_return_none(s in "[^aApPjJtT][a-zA-Z]{0,10}") {
        // Strings not starting with valid prefixes
        let result = OutputFormat::parse(&s);
        // If it happens to match "auto"/"plain"/"json"/"text" case-insensitively, it's valid
        let lower = s.to_lowercase();
        if lower == "auto" || lower == "plain" || lower == "json" || lower == "text" {
            prop_assert!(result.is_some());
        } else {
            prop_assert!(result.is_none());
        }
    }

    // 7. OutputFormat Clone and PartialEq are consistent
    #[test]
    fn output_format_clone_eq(fmt in arb_output_format()) {
        let cloned = fmt;
        prop_assert_eq!(fmt, cloned);
    }

    // 8. OutputFormat Debug produces non-empty string
    #[test]
    fn output_format_debug_non_empty(fmt in arb_output_format()) {
        let dbg = format!("{:?}", fmt);
        prop_assert!(!dbg.is_empty());
    }
}

// =============================================================================
// Style properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 9. Style disabled: all methods return input unchanged
    #[test]
    fn style_disabled_identity(text in arb_text()) {
        let style = Style::new(false);
        prop_assert_eq!(style.bold(&text), text.clone());
        prop_assert_eq!(style.dim(&text), text.clone());
        prop_assert_eq!(style.red(&text), text.clone());
        prop_assert_eq!(style.green(&text), text.clone());
        prop_assert_eq!(style.yellow(&text), text.clone());
        prop_assert_eq!(style.blue(&text), text.clone());
        prop_assert_eq!(style.cyan(&text), text.clone());
        prop_assert_eq!(style.gray(&text), text);
    }

    // 10. Style enabled: output contains original text
    #[test]
    fn style_enabled_preserves_text(text in arb_text()) {
        let style = Style::new(true);
        prop_assert!(style.bold(&text).contains(&text));
        prop_assert!(style.red(&text).contains(&text));
        prop_assert!(style.green(&text).contains(&text));
    }

    // 11. Style enabled: output starts with ESC and ends with RESET
    #[test]
    fn style_enabled_wraps_with_ansi(text in arb_text()) {
        let style = Style::new(true);
        let bold = style.bold(&text);
        prop_assert!(bold.starts_with("\x1b["));
        prop_assert!(bold.ends_with("\x1b[0m"));
    }

    // 12. Style enabled: output is longer than input (or equal for empty)
    #[test]
    fn style_enabled_output_longer(text in arb_text()) {
        let style = Style::new(true);
        let styled = style.bold(&text);
        prop_assert!(styled.len() >= text.len());
    }

    // 13. strip_ansi of styled text equals original
    #[test]
    fn strip_ansi_of_styled_text(text in arb_text()) {
        let style = Style::new(true);
        let styled = style.bold(&text);
        prop_assert_eq!(strip_ansi(&styled), text);
    }

    // 14. Style severity maps known severities deterministically
    #[test]
    fn severity_known_values_deterministic(sev in prop_oneof![
        Just("critical"), Just("error"), Just("warning"), Just("warn"), Just("info"),
        Just("CRITICAL"), Just("ERROR"), Just("WARNING"), Just("WARN"), Just("INFO"),
    ], text in arb_text()) {
        let style = Style::new(true);
        let result1 = style.severity(&text, sev);
        let result2 = style.severity(&text, sev);
        // Known severities should add ANSI codes
        prop_assert!(result1.contains("\x1b["));
        prop_assert_eq!(result1, result2);
    }

    // 15. Style severity unknown returns text unchanged (when enabled)
    #[test]
    fn severity_unknown_returns_plain(text in arb_text(), sev in "[a-z]{5,10}") {
        let lower = sev.to_lowercase();
        if lower != "critical" && lower != "error" && lower != "warning" && lower != "warn" && lower != "info" {
            let style = Style::new(true);
            let result = style.severity(&text, &sev);
            prop_assert_eq!(result, text);
        }
    }

    // 16. Style status: success is green, failure is red
    #[test]
    fn status_color_mapping(text in arb_text(), success in proptest::bool::ANY) {
        let style = Style::new(true);
        let result = style.status(&text, success);
        if success {
            prop_assert!(result.contains("\x1b[32m"), "success should be green");
        } else {
            prop_assert!(result.contains("\x1b[31m"), "failure should be red");
        }
    }
}

// =============================================================================
// strip_ansi properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 17. strip_ansi is idempotent
    #[test]
    fn strip_ansi_idempotent(s in arb_ansi_text()) {
        let once = strip_ansi(&s);
        let twice = strip_ansi(&once);
        prop_assert_eq!(once, twice);
    }

    // 18. strip_ansi of plain text is identity
    #[test]
    fn strip_ansi_plain_identity(text in arb_text()) {
        prop_assert_eq!(strip_ansi(&text), text);
    }

    // 19. strip_ansi output never contains ESC character
    #[test]
    fn strip_ansi_no_esc_in_output(s in arb_ansi_text()) {
        let stripped = strip_ansi(&s);
        prop_assert!(!stripped.contains('\x1b'), "stripped output should not contain ESC");
    }

    // 20. strip_ansi output length <= input length
    #[test]
    fn strip_ansi_output_shorter_or_equal(s in arb_ansi_text()) {
        let stripped = strip_ansi(&s);
        prop_assert!(stripped.len() <= s.len());
    }
}

// =============================================================================
// Table + Column properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 21. Column builder preserves all fields
    #[test]
    fn column_builder_preserves_fields(
        header in arb_header(),
        align in arb_alignment(),
        min_w in 0usize..100,
        max_w in 0usize..200,
    ) {
        let col = Column::new(header.clone())
            .align(align)
            .min_width(min_w)
            .max_width(max_w);
        prop_assert_eq!(&col.header, &header);
        prop_assert_eq!(col.min_width, min_w);
        prop_assert_eq!(col.max_width, max_w);
    }

    // 22. Table len tracks row count
    #[test]
    fn table_len_tracks_rows(n_rows in 0usize..20) {
        let mut table = Table::new(vec![Column::new("A")]).with_format(OutputFormat::Plain);
        for i in 0..n_rows {
            table.add_row(vec![format!("row{}", i)]);
        }
        prop_assert_eq!(table.len(), n_rows);
        prop_assert_eq!(table.is_empty(), n_rows == 0);
    }

    // 23. Table render in Plain mode contains all cell values
    #[test]
    fn table_plain_contains_all_cells(
        cells in prop::collection::vec(arb_text().prop_filter("non-empty", |s| !s.is_empty()), 1..5),
    ) {
        let cols: Vec<Column> = cells.iter().enumerate().map(|(i, _)| Column::new(format!("C{}", i))).collect();
        let mut table = Table::new(cols).with_format(OutputFormat::Plain);
        table.add_row(cells.clone());
        let output = table.render();
        for cell in &cells {
            let trimmed = cell.trim();
            if !trimmed.is_empty() {
                prop_assert!(output.contains(trimmed), "output should contain cell value: {}", trimmed);
            }
        }
    }

    // 24. Table render in JSON mode produces valid JSON array
    #[test]
    fn table_json_valid_json(
        n_rows in 1usize..5,
        n_cols in 1usize..4,
    ) {
        let cols: Vec<Column> = (0..n_cols).map(|i| Column::new(format!("Col{}", i))).collect();
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        for r in 0..n_rows {
            let row: Vec<String> = (0..n_cols).map(|c| format!("r{}c{}", r, c)).collect();
            table.add_row(row);
        }
        let output = table.render();
        let parsed: Result<Vec<serde_json::Value>, _> = serde_json::from_str(&output);
        prop_assert!(parsed.is_ok(), "JSON render should produce valid JSON");
        let arr = parsed.unwrap();
        prop_assert_eq!(arr.len(), n_rows);
    }

    // 25. Table JSON keys are lowercase with underscores
    #[test]
    fn table_json_keys_normalized(
        headers in prop::collection::vec(arb_header(), 1..4),
    ) {
        let cols: Vec<Column> = headers.iter().map(|h| Column::new(h.clone())).collect();
        let n = headers.len();
        let mut table = Table::new(cols).with_format(OutputFormat::Json);
        let row: Vec<String> = (0..n).map(|i| format!("val{}", i)).collect();
        table.add_row(row);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        for key in parsed[0].as_object().unwrap().keys() {
            prop_assert_eq!(key, &key.to_lowercase(), "JSON key should be lowercase");
            prop_assert!(!key.contains(' '), "JSON key should not contain spaces");
        }
    }

    // 26. Table empty renders header but no data rows
    #[test]
    fn table_empty_render_has_header_only(n_cols in 1usize..5) {
        let cols: Vec<Column> = (0..n_cols).map(|i| Column::new(format!("H{}", i))).collect();
        let table = Table::new(cols).with_format(OutputFormat::Plain);
        let output = table.render();
        let line_count = output.lines().count();
        // Should have exactly 1 line (header only, no data)
        prop_assert_eq!(line_count, 1, "empty table should render header only");
    }

    // 27. Table with min_width: rendered column is at least min_width wide
    #[test]
    fn table_min_width_respected(
        header in arb_header(),
        min_w in 1usize..30,
        cell_text in "[a-z]{1,5}",
    ) {
        let mut table = Table::new(vec![Column::new(header.clone()).min_width(min_w)])
            .with_format(OutputFormat::Plain);
        table.add_row(vec![cell_text]);
        let output = table.render();
        // The header line should be at least min_w characters (accounting for header text too)
        let first_line = output.lines().next().unwrap_or("");
        let visible = strip_ansi(first_line);
        let expected_min = min_w.max(header.len());
        prop_assert!(
            visible.len() >= expected_min,
            "first line {} should be >= {} wide", visible.len(), expected_min
        );
    }

    // 28. Table JSON strips ANSI from cell values
    #[test]
    fn table_json_strips_ansi(text in arb_text()) {
        let mut table = Table::new(vec![Column::new("Val")]).with_format(OutputFormat::Json);
        let ansi_text = format!("\x1b[31m{}\x1b[0m", text);
        table.add_row(vec![ansi_text]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        let val = parsed[0]["val"].as_str().unwrap();
        prop_assert!(!val.contains('\x1b'), "JSON values should not contain ANSI");
        prop_assert_eq!(val, text.as_str());
    }

    // 29. Table with_separator changes output
    #[test]
    fn table_custom_separator(
        sep in prop_oneof![Just(" | "), Just(" :: "), Just("\t")],
        cell1 in "[a-z]{3,8}",
        cell2 in "[a-z]{3,8}",
    ) {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B")])
            .with_format(OutputFormat::Plain)
            .with_separator(if sep == " | " { " | " } else if sep == " :: " { " :: " } else { "\t" });
        table.add_row(vec![cell1, cell2]);
        let output = table.render();
        // Data row should contain the separator
        let data_line = output.lines().nth(1).unwrap_or("");
        let expected_sep = if sep == " | " { " | " } else if sep == " :: " { " :: " } else { "\t" };
        prop_assert!(data_line.contains(expected_sep), "data line should contain separator");
    }

    // 30. Table with multiple rows: row count in JSON equals rows added
    #[test]
    fn table_json_row_count_matches(n_rows in 0usize..10) {
        let mut table = Table::new(vec![Column::new("X")]).with_format(OutputFormat::Json);
        for i in 0..n_rows {
            table.add_row(vec![format!("v{}", i)]);
        }
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        prop_assert_eq!(parsed.len(), n_rows);
    }
}
