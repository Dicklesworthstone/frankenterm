//! Golden corpus tests for pattern detection.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::patterns::PatternEngine;
use serde::Deserialize;
use serde_json::Value;

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

const DOGFOOD_MARKER: &str = "_dogfood_";

#[derive(Debug, Deserialize)]
struct DogfoodMeta {
    scenario: String,
    source: String,
    captured_at: String,
    platform: String,
    cross_platform: String,
    sanitized: bool,
}

fn is_dogfood_fixture(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.contains(DOGFOOD_MARKER))
}

fn read_dogfood_meta(fixture: &Path) -> DogfoodMeta {
    let meta_path = fixture.with_extension("meta.json");
    let meta_str = fs::read_to_string(&meta_path)
        .unwrap_or_else(|e| panic!("Missing metadata file {}: {e}", meta_path.display()));

    serde_json::from_str(&meta_str)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", meta_path.display()))
}

fn validate_dogfood_meta(meta: &DogfoodMeta, fixture: &Path) {
    assert!(
        !meta.scenario.trim().is_empty(),
        "Missing scenario in metadata for {}",
        fixture.display()
    );
    assert!(
        !meta.source.trim().is_empty(),
        "Missing source in metadata for {}",
        fixture.display()
    );
    assert!(
        meta.sanitized,
        "Dogfood fixture must be sanitized: {}",
        fixture.display()
    );
    assert!(
        matches!(meta.platform.as_str(), "macos" | "linux"),
        "Invalid platform '{}' in {} (expected 'macos' or 'linux')",
        meta.platform,
        fixture.display()
    );
    assert!(
        matches!(meta.cross_platform.as_str(), "pending" | "complete"),
        "Invalid cross_platform '{}' in {} (expected 'pending' or 'complete')",
        meta.cross_platform,
        fixture.display()
    );

    let captured_at = meta.captured_at.trim();
    let looks_like_iso8601 = captured_at.contains('T')
        && (captured_at.ends_with('Z')
            || captured_at.contains('+')
            || captured_at.rsplit_once('-').is_some());
    assert!(
        looks_like_iso8601,
        "captured_at must look like ISO-8601 in {} (got '{}')",
        fixture.display(),
        meta.captured_at
    );
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
fn dogfood_metadata_is_well_formed_and_cross_platform_consistent() {
    let base_dir = corpus_dir();
    let mut fixtures = Vec::new();
    collect_txt_files(&base_dir, &mut fixtures);
    fixtures.sort();

    let mut scenario_platforms: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut scenario_status: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for fixture in fixtures {
        if !is_dogfood_fixture(&fixture) {
            continue;
        }

        let meta = read_dogfood_meta(&fixture);
        validate_dogfood_meta(&meta, &fixture);

        scenario_platforms
            .entry(meta.scenario.clone())
            .or_default()
            .insert(meta.platform.clone());
        scenario_status
            .entry(meta.scenario)
            .or_default()
            .insert(meta.cross_platform);
    }

    assert!(
        !scenario_platforms.is_empty(),
        "Expected at least one dogfood fixture with metadata"
    );

    for (scenario, statuses) in &scenario_status {
        assert_eq!(
            statuses.len(),
            1,
            "Scenario '{}' has conflicting cross_platform states: {:?}",
            scenario,
            statuses
        );
    }

    for (scenario, platforms) in scenario_platforms {
        let status = scenario_status
            .get(&scenario)
            .and_then(|statuses| statuses.iter().next())
            .unwrap_or_else(|| panic!("Missing cross_platform status for scenario '{scenario}'"));

        match status.as_str() {
            "complete" => {
                let expected = BTreeSet::from([String::from("linux"), String::from("macos")]);
                assert_eq!(
                    platforms, expected,
                    "Scenario '{}' is complete but platforms are {:?}",
                    scenario, platforms
                );
            }
            "pending" => {
                assert_eq!(
                    platforms.len(),
                    1,
                    "Scenario '{}' is pending and must only have one captured platform, got {:?}",
                    scenario,
                    platforms
                );
            }
            _ => unreachable!(),
        }
    }
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
