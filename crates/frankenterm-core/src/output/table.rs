//! Table formatting utilities
//!
//! Provides a simple table formatter for CLI output with support for
//! alignment, column widths, and optional ANSI colors.

use super::format::{OutputFormat, Style};
use finl_unicode::grapheme_clusters::Graphemes;
use termwiz::cell::{grapheme_column_width, unicode_column_width};

// Unicode does not impose a useful upper bound on the number of combining
// scalars in one grapheme cluster. Keep terminal-cell truncation from becoming
// an unbounded byte-output path while retaining ample room for ordinary emoji
// ZWJ sequences and combining scripts.
const MAX_BYTES_PER_DISPLAY_CELL: usize = 256;

// Table cells are sometimes wrapped by `Style` before they reach this module.
// Preserve only those exact, balanced wrappers.  Treat every other escape
// sequence as untrusted input so OSC hyperlinks/title changes, arbitrary CSI,
// nested controls, and unterminated styling can never be re-emitted merely
// because the visible text happened to fit its column.
const SAFE_STYLE_PREFIXES: &[&str] = &[
    "\x1b[1m", "\x1b[2m", "\x1b[3m", "\x1b[4m", "\x1b[31m", "\x1b[32m", "\x1b[33m",
    "\x1b[34m", "\x1b[35m", "\x1b[36m", "\x1b[37m", "\x1b[90m", "\x1b[91m",
    "\x1b[92m", "\x1b[93m", "\x1b[94m", "\x1b[96m",
];
const SAFE_STYLE_RESET: &str = "\x1b[0m";

pub(super) fn visible_width(text: &str) -> usize {
    unicode_column_width(text, None)
}

pub(super) fn is_untrusted_display_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

fn replace_untrusted_display_controls(text: &str) -> String {
    text.chars()
        .map(|character| {
            if is_untrusted_display_control(character) {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn has_exact_safe_style_wrapper(text: &str) -> bool {
    let Some(inner_with_reset) = SAFE_STYLE_PREFIXES
        .iter()
        .find_map(|prefix| text.strip_prefix(*prefix))
    else {
        return false;
    };
    let Some(inner) = inner_with_reset.strip_suffix(SAFE_STYLE_RESET) else {
        return false;
    };

    !inner.contains('\x1b') && !inner.chars().any(is_untrusted_display_control)
}

pub(super) fn sanitize_terminal_text(text: &str) -> String {
    let stripped = strip_ansi(text);
    if stripped.chars().any(is_untrusted_display_control) {
        replace_untrusted_display_controls(&stripped)
    } else {
        stripped
    }
}

pub(super) fn prefix_within_width(text: &str, max_width: usize) -> &str {
    if max_width == 0 {
        return "";
    }

    let mut byte_end = 0usize;
    let mut width = 0usize;
    let max_bytes = max_width.saturating_mul(MAX_BYTES_PER_DISPLAY_CELL);
    for grapheme in Graphemes::new(text) {
        let grapheme_width = grapheme_column_width(grapheme, None);
        // A zero-cell grapheme at a table boundary can combine with or
        // otherwise alter the preceding separator even though it consumes no
        // advertised width. Ordinary combining text and emoji ZWJ sequences
        // remain intact because their complete grapheme has positive width;
        // only a disconnected/invisible cluster terminates the safe prefix.
        if grapheme_width == 0 {
            break;
        }
        let Some(next_width) = width.checked_add(grapheme_width) else {
            break;
        };
        let Some(next_byte_end) = byte_end.checked_add(grapheme.len()) else {
            break;
        };
        if next_width > max_width || next_byte_end > max_bytes {
            break;
        }
        width = next_width;
        byte_end = next_byte_end;
    }
    &text[..byte_end]
}

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
            .map(|col| visible_width(&sanitize_terminal_text(&col.header)).max(col.min_width))
            .collect();

        // Account for row content
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                let cell_len = visible_width(&sanitize_terminal_text(cell));
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
        if width == 0 {
            return String::new();
        }

        let preserve_safe_style = has_exact_safe_style_wrapper(cell);
        let stripped = strip_ansi(cell);
        let had_untrusted_controls = stripped.chars().any(is_untrusted_display_control);
        let stripped = if had_untrusted_controls {
            replace_untrusted_display_controls(&stripped)
        } else {
            stripped
        };
        let visible_len = visible_width(&stripped);
        let bounded_prefix = prefix_within_width(&stripped, width);

        if bounded_prefix.len() != stripped.len() {
            let truncated = if width > 3 {
                let mut truncated = prefix_within_width(&stripped, width - 3).to_string();
                truncated.push_str("...");
                truncated
            } else {
                bounded_prefix.to_string()
            };
            let truncated_width = visible_width(&truncated);
            return Self::align_cell(&truncated, width, truncated_width, alignment);
        }

        if visible_len >= width {
            return if preserve_safe_style {
                cell.to_string()
            } else {
                stripped
            };
        }

        let cell = if preserve_safe_style {
            cell
        } else {
            stripped.as_str()
        };
        Self::align_cell(cell, width, visible_len, alignment)
    }

    fn align_cell(cell: &str, width: usize, visible_len: usize, alignment: Alignment) -> String {
        let padding = width.saturating_sub(visible_len);
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
    #[derive(Clone, Copy)]
    enum State {
        Ground,
        Escape,
        EscapeIntermediate,
        Csi,
        Osc,
        OscEscape,
        ControlString,
        ControlStringEscape,
    }

    let mut result = String::with_capacity(s.len());
    let mut state = State::Ground;
    for c in s.chars() {
        state = match state {
            State::Ground => match c {
                '\x1b' => State::Escape,
                '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => State::ControlString,
                '\u{009b}' => State::Csi,
                '\u{009d}' => State::Osc,
                '\u{009c}' => State::Ground,
                _ => {
                    result.push(c);
                    State::Ground
                }
            },
            State::Escape => match c {
                '[' => State::Csi,
                ']' => State::Osc,
                'P' | 'X' | '^' | '_' => State::ControlString,
                '\x18' | '\x1a' => State::Ground,
                '\u{20}'..='\u{2f}' => State::EscapeIntermediate,
                _ => State::Ground,
            },
            State::EscapeIntermediate => match c {
                '\x18' | '\x1a' => State::Ground,
                '\u{20}'..='\u{2f}' => State::EscapeIntermediate,
                _ => State::Ground,
            },
            State::Csi => match c {
                '\x1b' => State::Escape,
                '\x18' | '\x1a' => State::Ground,
                '\u{40}'..='\u{7e}' => State::Ground,
                _ => State::Csi,
            },
            State::Osc => match c {
                '\x07' | '\u{009c}' => State::Ground,
                '\x1b' => State::OscEscape,
                '\x18' | '\x1a' => State::Ground,
                _ => State::Osc,
            },
            State::OscEscape => {
                if c == '\\' {
                    State::Ground
                } else if matches!(c, '\x18' | '\x1a') {
                    State::Ground
                } else if c == '\x1b' {
                    State::OscEscape
                } else {
                    State::Osc
                }
            }
            State::ControlString => match c {
                '\u{009c}' => State::Ground,
                '\x1b' => State::ControlStringEscape,
                '\x18' | '\x1a' => State::Ground,
                _ => State::ControlString,
            },
            State::ControlStringEscape => {
                if c == '\\' {
                    State::Ground
                } else if matches!(c, '\x18' | '\x1a') {
                    State::Ground
                } else if c == '\x1b' {
                    State::ControlStringEscape
                } else {
                    State::ControlString
                }
            }
        };
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
        assert_eq!(strip_ansi("a\x1b[1~b"), "ab");
        assert_eq!(strip_ansi("\x1b(Bascii"), "ascii");
        assert_eq!(
            strip_ansi("\x1b]8;;https://example.invalid\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(strip_ansi("\x1bPprivate payload\x1b\\shown"), "shown");
        assert_eq!(strip_ansi("prefix\x1b[31"), "prefix");
        assert_eq!(strip_ansi("hidden\x1b]title\x18visible"), "hiddenvisible");
        assert_eq!(strip_ansi("a\u{009b}31mb"), "ab");
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
    fn format_cell_truncation_width_3_uses_a_bounded_prefix() {
        // Width <= 3 has no room for an ellipsis, but still honors the bound.
        let result = Table::format_cell("abcdefg", 3, Alignment::Left);
        assert_eq!(result, "abc");
    }

    #[test]
    fn format_cell_width_4_truncation() {
        let result = Table::format_cell("abcdefg", 4, Alignment::Left);
        assert_eq!(result, "a...");
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn format_cell_unicode_truncation_and_padding_preserve_boundaries() {
        assert_eq!(
            Table::format_cell("héllo-world", 7, Alignment::Left),
            "héll..."
        );
        assert_eq!(
            Table::format_cell("é", 3, Alignment::Right),
            "  é",
            "padding must use terminal-cell width rather than UTF-8 bytes"
        );
        assert_eq!(Table::format_cell("表ab", 3, Alignment::Left), "表a");
        assert_eq!(visible_width(&Table::format_cell("表ab", 3, Alignment::Left)), 3);

        let combining_spam = "\u{0301}".repeat(1_024);
        assert_eq!(
            Table::format_cell(&combining_spam, 4, Alignment::Left),
            "... "
        );
        assert_eq!(
            Table::format_cell("\u{0301}", 4, Alignment::Left),
            "... ",
            "an orphan combining mark must not attach to the preceding separator"
        );
        assert_eq!(
            Table::format_cell("a\u{0301}", 2, Alignment::Left),
            "a\u{0301} ",
            "a combining mark attached to a positive-width base remains intact"
        );
    }

    #[test]
    fn format_cell_neutralizes_untrusted_single_line_controls() {
        assert_eq!(
            Table::format_cell("\x1b[31mline one\nline two\u{202e}x\x1b[0m", 25, Alignment::Left),
            "line one line two x      "
        );
    }

    #[test]
    fn format_cell_preserves_only_exact_balanced_project_style_wrappers() {
        assert_eq!(
            Table::format_cell("\x1b[32mOK\x1b[0m", 4, Alignment::Left),
            "\x1b[32mOK\x1b[0m  "
        );
        assert_eq!(
            Table::format_cell("\x1b]0;stolen title\x07OK", 4, Alignment::Left),
            "OK  ",
            "OSC must never be restored after width calculation"
        );
        assert_eq!(
            Table::format_cell("\x1b[31mred without reset", 20, Alignment::Left),
            "red without reset   ",
            "unterminated styling must not leak into following output"
        );
        assert_eq!(
            Table::format_cell("\x1b[32msafe\x1b]8;;https://example.invalid\x07x\x1b[0m", 8, Alignment::Left),
            "safex   ",
            "a safe-looking outer wrapper must not bless nested controls"
        );
    }

    #[test]
    fn format_cell_empty_string() {
        let result = Table::format_cell("", 5, Alignment::Left);
        assert_eq!(result, "     ");
    }

    #[test]
    fn format_cell_zero_width() {
        assert_eq!(Table::format_cell("", 0, Alignment::Left), "");
        assert_eq!(Table::format_cell("hidden", 0, Alignment::Left), "");
        assert_eq!(Table::format_cell("\x1b[31m", 0, Alignment::Left), "");
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
        // ESC X is the ANSI Start of String control; without a terminator its
        // payload is untrusted control data and is stripped through EOF.
        let result = strip_ansi("\x1bXhello");
        assert_eq!(result, "");
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
