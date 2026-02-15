//! Table formatting utilities
//!
//! Provides a simple table formatter for CLI output with support for
//! alignment, column widths, and optional ANSI colors.

use super::format::{OutputFormat, Style};

/// Column alignment
#[derive(Debug, Clone, Copy, Default)]
pub enum Alignment {
    /// Left-aligned (default)
    #[default]
    Left,
    /// Right-aligned
    Right,
    /// Center-aligned
    Center,
}

/// Table column definition
#[derive(Debug, Clone)]
pub struct Column {
    /// Column header
    pub header: String,
    /// Column alignment
    pub alignment: Alignment,
    /// Minimum width (0 = auto)
    pub min_width: usize,
    /// Maximum width (0 = unlimited)
    pub max_width: usize,
}

impl Column {
    /// Create a new column with default settings
    #[must_use]
    pub fn new(header: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            alignment: Alignment::Left,
            min_width: 0,
            max_width: 0,
        }
    }

    /// Set column alignment
    #[must_use]
    pub fn align(mut self, alignment: Alignment) -> Self {
        self.alignment = alignment;
        self
    }

    /// Set minimum width
    #[must_use]
    pub fn min_width(mut self, width: usize) -> Self {
        self.min_width = width;
        self
    }

    /// Set maximum width
    #[must_use]
    pub fn max_width(mut self, width: usize) -> Self {
        self.max_width = width;
        self
    }
}

/// Table formatter
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<String>>,
    format: OutputFormat,
    separator: &'static str,
}

impl Table {
    /// Create a new table with the given columns
    #[must_use]
    pub fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            format: OutputFormat::Auto,
            separator: "  ",
        }
    }

    /// Set the output format
    #[must_use]
    pub fn with_format(mut self, format: OutputFormat) -> Self {
        self.format = format;
        self
    }

    /// Set the column separator
    #[must_use]
    pub fn with_separator(mut self, separator: &'static str) -> Self {
        self.separator = separator;
        self
    }

    /// Add a row to the table
    pub fn add_row(&mut self, cells: Vec<impl Into<String>>) {
        let row: Vec<String> = cells.into_iter().map(Into::into).collect();
        assert_eq!(
            row.len(),
            self.columns.len(),
            "Row has {} cells, expected {}",
            row.len(),
            self.columns.len()
        );
        self.rows.push(row);
    }

    /// Calculate column widths based on content
    fn calculate_widths(&self) -> Vec<usize> {
        let mut widths: Vec<usize> = self
            .columns
            .iter()
            .map(|col| col.header.len().max(col.min_width))
            .collect();

        // Account for row content
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                let cell_len = strip_ansi(cell).len();
                widths[i] = widths[i].max(cell_len);
            }
        }

        // Apply max width constraints
        for (i, col) in self.columns.iter().enumerate() {
            if col.max_width > 0 && widths[i] > col.max_width {
                widths[i] = col.max_width;
            }
        }

        widths
    }

    /// Format a cell with the given width and alignment
    fn format_cell(cell: &str, width: usize, alignment: Alignment) -> String {
        let visible_len = strip_ansi(cell).len();

        if visible_len >= width {
            // Truncate if needed
            let stripped = strip_ansi(cell);
            if stripped.len() > width && width > 3 {
                return format!("{}...", &stripped[..width - 3]);
            }
            return cell.to_string();
        }

        let padding = width - visible_len;
        match alignment {
            Alignment::Left => format!("{cell}{}", " ".repeat(padding)),
            Alignment::Right => format!("{}{cell}", " ".repeat(padding)),
            Alignment::Center => {
                let left = padding / 2;
                let right = padding - left;
                format!("{}{cell}{}", " ".repeat(left), " ".repeat(right))
            }
        }
    }

    /// Render the table as a string
    #[must_use]
    pub fn render(&self) -> String {
        if self.format.is_json() {
            return self.render_json();
        }

        let widths = self.calculate_widths();
        let style = Style::from_format(self.format);
        let mut output = String::new();

        // Header row
        let header: Vec<String> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let formatted = Self::format_cell(&col.header, widths[i], col.alignment);
                style.bold(&formatted)
            })
            .collect();
        output.push_str(&header.join(self.separator));
        output.push('\n');

        // Separator line (only for rich output)
        if self.format.is_rich() {
            let sep_line: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            output.push_str(&style.dim(&sep_line.join(self.separator)));
            output.push('\n');
        }

        // Data rows
        for row in &self.rows {
            let formatted: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| Self::format_cell(cell, widths[i], self.columns[i].alignment))
                .collect();
            output.push_str(&formatted.join(self.separator));
            output.push('\n');
        }

        output
    }

    /// Render the table as JSON array
    fn render_json(&self) -> String {
        let records: Vec<serde_json::Value> = self
            .rows
            .iter()
            .map(|row| {
                let mut obj = serde_json::Map::new();
                for (i, cell) in row.iter().enumerate() {
                    let key = self.columns[i].header.to_lowercase().replace(' ', "_");
                    obj.insert(key, serde_json::Value::String(strip_ansi(cell)));
                }
                serde_json::Value::Object(obj)
            })
            .collect();

        serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".to_string())
    }

    /// Check if the table is empty
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Get the number of rows
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Strip ANSI escape codes from a string
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip escape sequence
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // Skip until we hit a letter (the command character)
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_basic() {
        let mut table = Table::new(vec![
            Column::new("ID"),
            Column::new("Name"),
            Column::new("Status"),
        ])
        .with_format(OutputFormat::Plain);

        table.add_row(vec!["1", "Alice", "Active"]);
        table.add_row(vec!["2", "Bob", "Inactive"]);

        let output = table.render();
        assert!(output.contains("ID"));
        assert!(output.contains("Alice"));
        assert!(output.contains("Bob"));
    }

    #[test]
    fn test_strip_ansi() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1m\x1b[32mbold green\x1b[0m"), "bold green");
    }

    #[test]
    fn test_column_alignment() {
        let formatted = Table::format_cell("test", 10, Alignment::Left);
        assert_eq!(formatted, "test      ");

        let formatted = Table::format_cell("test", 10, Alignment::Right);
        assert_eq!(formatted, "      test");

        let formatted = Table::format_cell("test", 10, Alignment::Center);
        assert_eq!(formatted, "   test   ");
    }

    #[test]
    fn test_table_json() {
        let mut table = Table::new(vec![Column::new("ID"), Column::new("Name")])
            .with_format(OutputFormat::Json);

        table.add_row(vec!["1", "Alice"]);

        let output = table.render();
        assert!(output.contains("\"id\""));
        assert!(output.contains("\"name\""));
        assert!(output.contains("\"1\""));
        assert!(output.contains("\"Alice\""));
    }

    // =====================================================================
    // Column builder tests
    // =====================================================================

    #[test]
    fn column_new_defaults() {
        let col = Column::new("Test");
        assert_eq!(col.header, "Test");
        assert_eq!(col.min_width, 0);
        assert_eq!(col.max_width, 0);
        assert!(matches!(col.alignment, Alignment::Left));
    }

    #[test]
    fn column_builder_chain() {
        let col = Column::new("Price")
            .align(Alignment::Right)
            .min_width(8)
            .max_width(20);
        assert_eq!(col.header, "Price");
        assert_eq!(col.min_width, 8);
        assert_eq!(col.max_width, 20);
        assert!(matches!(col.alignment, Alignment::Right));
    }

    #[test]
    fn column_center_alignment() {
        let col = Column::new("Status").align(Alignment::Center);
        assert!(matches!(col.alignment, Alignment::Center));
    }

    #[test]
    fn column_from_various_string_types() {
        let col_str = Column::new("header");
        assert_eq!(col_str.header, "header");

        let col_string = Column::new(String::from("header2"));
        assert_eq!(col_string.header, "header2");
    }

    // =====================================================================
    // Table builder and metadata tests
    // =====================================================================

    #[test]
    fn table_empty() {
        let table = Table::new(vec![Column::new("A"), Column::new("B")]);
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn table_len_after_adds() {
        let mut table = Table::new(vec![Column::new("X")]);
        table.add_row(vec!["1"]);
        table.add_row(vec!["2"]);
        table.add_row(vec!["3"]);
        assert_eq!(table.len(), 3);
        assert!(!table.is_empty());
    }

    #[test]
    fn table_with_separator() {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B")])
            .with_format(OutputFormat::Plain)
            .with_separator(" | ");
        table.add_row(vec!["x", "y"]);
        let output = table.render();
        assert!(output.contains(" | "), "Should use custom separator");
    }

    #[test]
    #[should_panic(expected = "Row has 2 cells, expected 3")]
    fn add_row_wrong_column_count_panics() {
        let mut table = Table::new(vec![Column::new("A"), Column::new("B"), Column::new("C")]);
        table.add_row(vec!["only", "two"]);
    }

    // =====================================================================
    // format_cell alignment tests
    // =====================================================================

    #[test]
    fn format_cell_exact_width() {
        let result = Table::format_cell("abcd", 4, Alignment::Left);
        assert_eq!(result, "abcd");
    }

    #[test]
    fn format_cell_left_padding() {
        let result = Table::format_cell("hi", 6, Alignment::Left);
        assert_eq!(result, "hi    ");
    }

    #[test]
    fn format_cell_right_padding() {
        let result = Table::format_cell("hi", 6, Alignment::Right);
        assert_eq!(result, "    hi");
    }

    #[test]
    fn format_cell_center_padding_even() {
        let result = Table::format_cell("ab", 6, Alignment::Center);
        assert_eq!(result, "  ab  ");
    }

    #[test]
    fn format_cell_center_padding_odd() {
        // "abc" is 3 chars, width 6 => padding=3, left=1, right=2
        let result = Table::format_cell("abc", 6, Alignment::Center);
        assert_eq!(result.len(), 6);
        assert!(result.contains("abc"));
    }

    #[test]
    fn format_cell_truncation_with_ellipsis() {
        let result = Table::format_cell("a very long string here", 10, Alignment::Left);
        assert_eq!(result.len(), 10);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn format_cell_truncation_width_3_no_ellipsis() {
        // Width <= 3 means no room for "..." so just return cell as-is
        let result = Table::format_cell("abcdefg", 3, Alignment::Left);
        assert_eq!(result, "abcdefg");
    }

    #[test]
    fn format_cell_width_4_truncation() {
        let result = Table::format_cell("abcdefg", 4, Alignment::Left);
        assert_eq!(result, "a...");
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn format_cell_empty_string() {
        let result = Table::format_cell("", 5, Alignment::Left);
        assert_eq!(result, "     ");
    }

    #[test]
    fn format_cell_zero_width() {
        // Visible len (0) >= width (0), so returns cell as-is
        let result = Table::format_cell("", 0, Alignment::Left);
        assert_eq!(result, "");
    }

    // =====================================================================
    // strip_ansi tests
    // =====================================================================

    #[test]
    fn strip_ansi_no_escapes() {
        assert_eq!(strip_ansi("hello world"), "hello world");
    }

    #[test]
    fn strip_ansi_single_color() {
        assert_eq!(strip_ansi("\x1b[31mred text\x1b[0m"), "red text");
    }

    #[test]
    fn strip_ansi_nested_codes() {
        assert_eq!(strip_ansi("\x1b[1m\x1b[31mbold red\x1b[0m"), "bold red");
    }

    #[test]
    fn strip_ansi_multi_param_code() {
        assert_eq!(strip_ansi("\x1b[38;5;196mcolor\x1b[0m"), "color");
    }

    #[test]
    fn strip_ansi_empty_string() {
        assert_eq!(strip_ansi(""), "");
    }

    #[test]
    fn strip_ansi_only_escape_codes() {
        assert_eq!(strip_ansi("\x1b[31m\x1b[0m"), "");
    }

    #[test]
    fn strip_ansi_escape_without_bracket() {
        // ESC not followed by '[' — just the ESC is consumed, rest preserved
        let result = strip_ansi("\x1bXhello");
        assert_eq!(result, "Xhello");
    }

    #[test]
    fn strip_ansi_preserves_non_escape_special_chars() {
        assert_eq!(strip_ansi("tab\there"), "tab\there");
        assert_eq!(strip_ansi("line\nbreak"), "line\nbreak");
    }

    // =====================================================================
    // calculate_widths tests
    // =====================================================================

    #[test]
    fn calculate_widths_header_only() {
        let table = Table::new(vec![Column::new("Name"), Column::new("ID")]);
        let widths = table.calculate_widths();
        assert_eq!(widths, vec![4, 2]); // "Name"=4, "ID"=2
    }

    #[test]
    fn calculate_widths_respects_min_width() {
        let table = Table::new(vec![Column::new("A").min_width(10)]);
        let widths = table.calculate_widths();
        assert_eq!(widths, vec![10]);
    }

    #[test]
    fn calculate_widths_respects_max_width() {
        let mut table = Table::new(vec![Column::new("X").max_width(5)]);
        table.add_row(vec!["a very long cell value"]);
        let widths = table.calculate_widths();
        assert_eq!(widths, vec![5]);
    }

    #[test]
    fn calculate_widths_content_wider_than_header() {
        let mut table = Table::new(vec![Column::new("ID")]);
        table.add_row(vec!["12345"]);
        let widths = table.calculate_widths();
        assert_eq!(widths, vec![5]); // "12345" > "ID"
    }

    #[test]
    fn calculate_widths_ansi_not_counted() {
        let mut table = Table::new(vec![Column::new("Status")]);
        table.add_row(vec!["\x1b[32mOK\x1b[0m"]);
        let widths = table.calculate_widths();
        // "Status"=6, "\x1b[32mOK\x1b[0m" visible is "OK"=2, so max is 6
        assert_eq!(widths, vec![6]);
    }

    // =====================================================================
    // Render tests
    // =====================================================================

    #[test]
    fn render_plain_no_separator_line() {
        let mut table = Table::new(vec![Column::new("Col")]).with_format(OutputFormat::Plain);
        table.add_row(vec!["val"]);
        let output = table.render();
        assert!(
            !output.contains('─'),
            "Plain format should not have separator lines"
        );
    }

    #[test]
    fn render_plain_contains_all_data() {
        let mut table =
            Table::new(vec![Column::new("A"), Column::new("B")]).with_format(OutputFormat::Plain);
        table.add_row(vec!["hello", "world"]);
        table.add_row(vec!["foo", "bar"]);
        let output = table.render();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
        assert!(output.contains("foo"));
        assert!(output.contains("bar"));
    }

    #[test]
    fn render_json_valid_array() {
        let mut table = Table::new(vec![Column::new("Name"), Column::new("Age")])
            .with_format(OutputFormat::Json);
        table.add_row(vec!["Alice", "30"]);
        table.add_row(vec!["Bob", "25"]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> =
            serde_json::from_str(&output).expect("should be valid JSON array");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["name"], "Alice");
        assert_eq!(parsed[0]["age"], "30");
        assert_eq!(parsed[1]["name"], "Bob");
    }

    #[test]
    fn render_json_header_normalization() {
        let mut table = Table::new(vec![Column::new("Full Name"), Column::new("Pane ID")])
            .with_format(OutputFormat::Json);
        table.add_row(vec!["test", "42"]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert!(parsed[0].get("full_name").is_some(), "spaces → underscores");
        assert!(parsed[0].get("pane_id").is_some());
    }

    #[test]
    fn render_json_empty_table() {
        let table = Table::new(vec![Column::new("A")]).with_format(OutputFormat::Json);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn render_json_strips_ansi_from_cells() {
        let mut table = Table::new(vec![Column::new("V")]).with_format(OutputFormat::Json);
        table.add_row(vec!["\x1b[31mred\x1b[0m"]);
        let output = table.render();
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed[0]["v"], "red", "ANSI should be stripped in JSON");
    }

    #[test]
    fn render_multiple_columns_alignment() {
        let mut table = Table::new(vec![
            Column::new("Left").align(Alignment::Left),
            Column::new("Right").align(Alignment::Right),
            Column::new("Center").align(Alignment::Center),
        ])
        .with_format(OutputFormat::Plain);
        table.add_row(vec!["a", "b", "c"]);
        let output = table.render();
        // Just verify it renders without panic and contains data
        assert!(output.contains('a'));
        assert!(output.contains('b'));
        assert!(output.contains('c'));
    }

    #[test]
    fn render_single_column_single_row() {
        let mut table = Table::new(vec![Column::new("Only")]).with_format(OutputFormat::Plain);
        table.add_row(vec!["value"]);
        let rendered = table.render();
        assert!(rendered.lines().count() >= 2); // header + data row
    }
}
