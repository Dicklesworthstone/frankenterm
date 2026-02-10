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
}
