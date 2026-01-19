//! Golden corpus tests for pattern detection.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use wa_core::patterns::PatternEngine;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("corpus")
}

fn collect_txt_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_txt_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "txt") {
            out.push(path);
        }
    }
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let mut sorted = std::collections::BTreeMap::new();
            for (key, val) in map {
                sorted.insert(key.clone(), canonicalize(val));
            }
            let mut out = serde_json::Map::new();
            for (key, val) in sorted {
                out.insert(key, val);
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn snippet(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", &text[..max_len])
    }
}

fn extract_rule_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("rule_id"))
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn corpus_fixtures_match_expected() {
    let base_dir = corpus_dir();
    let mut fixtures = Vec::new();
    collect_txt_files(&base_dir, &mut fixtures);
    fixtures.sort();

    let engine = PatternEngine::new();

    for fixture in fixtures {
        let input = fs::read_to_string(&fixture)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", fixture.display()));

        let expected_path = fixture.with_extension("expect.json");
        let expected_str = fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("Missing expected file {}: {e}", expected_path.display()));

        let detections = engine.detect(&input);
        let actual_value =
            serde_json::to_value(&detections).expect("Failed to serialize detections");
        let expected_value: Value = serde_json::from_str(&expected_str)
            .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", expected_path.display()));

        let actual_norm = canonicalize(&actual_value);
        let expected_norm = canonicalize(&expected_value);

        if actual_norm != expected_norm {
            let rel = fixture.strip_prefix(&base_dir).unwrap_or(&fixture);
            let expected_ids = extract_rule_ids(&expected_norm);
            let actual_ids = detections
                .iter()
                .map(|d| d.rule_id.clone())
                .collect::<Vec<_>>();
            let preview = snippet(&input, 200);
            let expected_json = serde_json::to_string_pretty(&expected_norm)
                .unwrap_or_else(|_| "<failed to serialize expected>".to_string());
            let actual_json = serde_json::to_string_pretty(&actual_norm)
                .unwrap_or_else(|_| "<failed to serialize actual>".to_string());

            panic!(
                "Corpus mismatch for {}\nExpected rules: {:?}\nActual rules: {:?}\nInput snippet: {}\nExpected JSON: {}\nActual JSON: {}",
                rel.display(),
                expected_ids,
                actual_ids,
                preview,
                expected_json,
                actual_json
            );
        }
    }
}
