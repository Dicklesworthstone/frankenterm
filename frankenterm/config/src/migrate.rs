//! Migration from `wezterm.lua` to `frankenterm.toml`.
//!
//! Best-effort conversion of Lua configs to TOML format. Static assignments
//! are converted directly; dynamic constructs (conditionals, callbacks) are
//! preserved as `[MANUAL]` comments for the user to review.

use anyhow::{Context, Result};
use frankenterm_dynamic::Value;
use std::path::Path;

/// Result of a migration attempt.
#[derive(Clone, Debug)]
pub struct MigrationResult {
    /// The generated TOML content.
    pub toml_content: String,
    /// Config keys that were successfully converted.
    pub converted: Vec<String>,
    /// Config keys or constructs that require manual review.
    pub manual_review: Vec<ManualItem>,
    /// Source file path.
    pub source: std::path::PathBuf,
}

/// An item that couldn't be automatically converted.
#[derive(Clone, Debug)]
pub struct ManualItem {
    pub key: String,
    pub reason: String,
    pub original_snippet: Option<String>,
}

/// Migrate a wezterm.lua config to frankenterm.toml format.
///
/// This uses the dynamic value representation: load the Lua config via the
/// existing config pipeline, then serialize the resulting dynamic value to
/// TOML. Fields that can't be represented in TOML get `[MANUAL]` comments.
pub fn migrate_config(dynamic: &Value, source_path: &Path) -> Result<MigrationResult> {
    let mut toml_lines = Vec::new();
    let mut converted = Vec::new();
    let mut manual_review = Vec::new();

    // Header
    toml_lines.push(format!(
        "# Migrated from {} on {}",
        source_path.display(),
        chrono_lite_date(),
    ));
    toml_lines
        .push("# Some settings may require manual review. See [MANUAL] comments.".to_string());
    toml_lines.push(String::new());

    match dynamic {
        Value::Object(obj) => {
            // Separate simple values from nested tables
            let mut simple_keys = Vec::new();
            let mut table_keys = Vec::new();

            for (k, v) in obj.iter() {
                let key_str = value_to_key_string(k);
                match v {
                    Value::Object(_) | Value::Array(_) => table_keys.push((key_str, v)),
                    _ => simple_keys.push((key_str, v)),
                }
            }

            // Write simple key-value pairs first
            for (key, value) in &simple_keys {
                match value_to_toml_value(value) {
                    Ok(toml_str) => {
                        toml_lines.push(format!("{key} = {toml_str}"));
                        converted.push(key.clone());
                    }
                    Err(reason) => {
                        toml_lines.push(format!("# [MANUAL] {key}: {reason}"));
                        manual_review.push(ManualItem {
                            key: key.clone(),
                            reason,
                            original_snippet: Some(format!("{value:?}")),
                        });
                    }
                }
            }

            if !simple_keys.is_empty() && !table_keys.is_empty() {
                toml_lines.push(String::new());
            }

            // Write nested tables
            for (key, value) in &table_keys {
                match value {
                    Value::Object(inner) => {
                        toml_lines.push(format!("[{key}]"));
                        write_object_fields(
                            inner,
                            key,
                            &mut toml_lines,
                            &mut converted,
                            &mut manual_review,
                        );
                        toml_lines.push(String::new());
                    }
                    Value::Array(items) => {
                        for item in items.iter() {
                            if let Value::Object(inner) = item {
                                toml_lines.push(format!("[[{key}]]"));
                                write_object_fields(
                                    inner,
                                    key,
                                    &mut toml_lines,
                                    &mut converted,
                                    &mut manual_review,
                                );
                                toml_lines.push(String::new());
                            } else {
                                // Array of non-objects
                                match value_to_toml_value(value) {
                                    Ok(toml_str) => {
                                        toml_lines.push(format!("{key} = {toml_str}"));
                                        converted.push(key.clone());
                                    }
                                    Err(reason) => {
                                        toml_lines.push(format!("# [MANUAL] {key}: {reason}"));
                                        manual_review.push(ManualItem {
                                            key: key.clone(),
                                            reason,
                                            original_snippet: None,
                                        });
                                    }
                                }
                                break;
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
        _ => {
            manual_review.push(ManualItem {
                key: "(root)".into(),
                reason: "Config root is not an object".into(),
                original_snippet: Some(format!("{dynamic:?}")),
            });
        }
    }

    let toml_content = toml_lines.join("\n") + "\n";

    Ok(MigrationResult {
        toml_content,
        converted,
        manual_review,
        source: source_path.to_path_buf(),
    })
}

/// Write a migration result to a file.
pub fn write_migration(result: &MigrationResult, output_path: &Path) -> Result<()> {
    std::fs::write(output_path, &result.toml_content).with_context(|| {
        format!(
            "failed to write migrated config to {}",
            output_path.display()
        )
    })
}

fn write_object_fields(
    obj: &frankenterm_dynamic::Object,
    parent_key: &str,
    lines: &mut Vec<String>,
    converted: &mut Vec<String>,
    manual: &mut Vec<ManualItem>,
) {
    for (k, v) in obj.iter() {
        let key_str = value_to_key_string(k);
        let full_key = format!("{parent_key}.{key_str}");

        match v {
            Value::Object(_) | Value::Array(_) => {
                // Nested table within a table — use dotted keys
                lines.push(format!(
                    "# [MANUAL] {key_str}: nested table (review manually)"
                ));
                manual.push(ManualItem {
                    key: full_key,
                    reason: "deeply nested table".into(),
                    original_snippet: None,
                });
            }
            _ => match value_to_toml_value(v) {
                Ok(toml_str) => {
                    lines.push(format!("{key_str} = {toml_str}"));
                    converted.push(full_key);
                }
                Err(reason) => {
                    lines.push(format!("# [MANUAL] {key_str}: {reason}"));
                    manual.push(ManualItem {
                        key: full_key,
                        reason,
                        original_snippet: Some(format!("{v:?}")),
                    });
                }
            },
        }
    }
}

/// Convert a dynamic Value key to a TOML key string.
fn value_to_key_string(v: &Value) -> String {
    match v {
        Value::String(s) => {
            if needs_quoting(s) {
                format!("\"{}\"", escape_toml_string(s))
            } else {
                s.clone()
            }
        }
        other => format!("{other:?}"),
    }
}

/// Convert a dynamic Value to a TOML value string.
fn value_to_toml_value(v: &Value) -> Result<String, String> {
    match v {
        Value::Bool(b) => Ok(format!("{b}")),
        Value::String(s) => Ok(format!("\"{}\"", escape_toml_string(s))),
        Value::I64(n) => Ok(format!("{n}")),
        Value::U64(n) => Ok(format!("{n}")),
        Value::F64(f) => {
            let f_val: f64 = (*f).into();
            Ok(format!("{f_val}"))
        }
        Value::Null => Err("null value (not representable in TOML)".into()),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items.iter() {
                parts.push(value_to_toml_value(item)?);
            }
            Ok(format!("[{}]", parts.join(", ")))
        }
        Value::Object(_) => Err("inline table (use [table] section instead)".into()),
    }
}

/// Whether a TOML key needs quoting.
fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.contains(' ')
        || s.contains('.')
        || s.contains('"')
        || s.contains('\\')
        || s.starts_with(|c: char| c.is_ascii_digit())
}

/// Escape special characters for TOML strings.
fn escape_toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// Simple date string without pulling in chrono.
fn chrono_lite_date() -> String {
    // Use std::time for a basic date stamp
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();

    // Basic conversion: seconds since epoch to YYYY-MM-DD
    let days = secs / 86400;
    let mut year = 1970;
    let mut remaining_days = days;

    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if remaining_days < days_in_year {
            break;
        }
        remaining_days -= days_in_year;
        year += 1;
    }

    let month_days = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1;
    for &md in &month_days {
        if remaining_days < md {
            break;
        }
        remaining_days -= md;
        month += 1;
    }
    let day = remaining_days + 1;

    format!("{year}-{month:02}-{day:02}")
}

fn is_leap_year(year: u64) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_obj(entries: Vec<(&str, Value)>) -> Value {
        let mut map: BTreeMap<Value, Value> = BTreeMap::new();
        for (k, v) in entries {
            map.insert(Value::String(k.to_string()), v);
        }
        Value::Object(map.into())
    }

    #[test]
    fn migrate_simple_config() {
        let config = make_obj(vec![
            ("font_size", Value::F64(ordered_float::OrderedFloat(14.0))),
            (
                "color_scheme",
                Value::String("Catppuccin Mocha".to_string()),
            ),
            ("scrollback_lines", Value::U64(10000)),
            ("enable_tab_bar", Value::Bool(false)),
        ]);

        let result = migrate_config(&config, Path::new("~/.wezterm.lua")).unwrap();

        assert!(result.toml_content.contains("font_size = 14"));
        assert!(result
            .toml_content
            .contains("color_scheme = \"Catppuccin Mocha\""));
        assert!(result.toml_content.contains("scrollback_lines = 10000"));
        assert!(result.toml_content.contains("enable_tab_bar = false"));
        assert_eq!(result.converted.len(), 4);
        assert!(result.manual_review.is_empty());
    }

    #[test]
    fn migrate_nested_table() {
        let ssh = make_obj(vec![
            ("name", Value::String("work".to_string())),
            (
                "remote_address",
                Value::String("work.example.com".to_string()),
            ),
        ]);
        let config = make_obj(vec![(
            "ssh_domains",
            Value::Array(vec![ssh].into_iter().collect()),
        )]);

        let result = migrate_config(&config, Path::new("test.lua")).unwrap();
        assert!(result.toml_content.contains("[[ssh_domains]]"));
        assert!(result.toml_content.contains("name = \"work\""));
        assert!(result
            .toml_content
            .contains("remote_address = \"work.example.com\""));
    }

    #[test]
    fn migrate_null_value_produces_manual() {
        let config = make_obj(vec![("custom_callback", Value::Null)]);

        let result = migrate_config(&config, Path::new("test.lua")).unwrap();
        assert!(!result.manual_review.is_empty());
        assert!(result.toml_content.contains("[MANUAL]"));
    }

    #[test]
    fn migrate_empty_config() {
        let config = make_obj(vec![]);

        let result = migrate_config(&config, Path::new("test.lua")).unwrap();
        assert!(result.converted.is_empty());
        assert!(result.manual_review.is_empty());
        // Should still have the header
        assert!(result.toml_content.contains("# Migrated from"));
    }

    #[test]
    fn migrate_array_of_primitives() {
        let config = make_obj(vec![(
            "tags",
            Value::Array(
                vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                ]
                .into_iter()
                .collect(),
            ),
        )]);

        let result = migrate_config(&config, Path::new("test.lua")).unwrap();
        assert!(result.toml_content.contains("tags = [\"a\", \"b\"]"));
    }

    #[test]
    fn escape_special_chars() {
        assert_eq!(escape_toml_string("hello"), "hello");
        assert_eq!(escape_toml_string("he\"llo"), "he\\\"llo");
        assert_eq!(escape_toml_string("line\nnew"), "line\\nnew");
        assert_eq!(escape_toml_string("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn needs_quoting_checks() {
        assert!(!needs_quoting("font_size"));
        assert!(needs_quoting("has space"));
        assert!(needs_quoting("has.dot"));
        assert!(needs_quoting(""));
        assert!(needs_quoting("123start"));
    }

    #[test]
    fn toml_value_roundtrip() {
        assert_eq!(value_to_toml_value(&Value::Bool(true)).unwrap(), "true");
        assert_eq!(
            value_to_toml_value(&Value::String("hi".into())).unwrap(),
            "\"hi\""
        );
        assert_eq!(value_to_toml_value(&Value::I64(42)).unwrap(), "42");
        assert_eq!(value_to_toml_value(&Value::U64(100)).unwrap(), "100");
        assert!(value_to_toml_value(&Value::Null).is_err());
    }

    #[test]
    fn write_migration_creates_file() {
        let config = make_obj(vec![(
            "font_size",
            Value::F64(ordered_float::OrderedFloat(12.0)),
        )]);
        let result = migrate_config(&config, Path::new("test.lua")).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("frankenterm.toml");
        write_migration(&result, &output).unwrap();

        let content = std::fs::read_to_string(&output).unwrap();
        assert!(content.contains("font_size = 12"));
    }

    #[test]
    fn date_generation_produces_valid_format() {
        let date = chrono_lite_date();
        // Should match YYYY-MM-DD format
        assert_eq!(date.len(), 10);
        assert_eq!(&date[4..5], "-");
        assert_eq!(&date[7..8], "-");
    }
}
