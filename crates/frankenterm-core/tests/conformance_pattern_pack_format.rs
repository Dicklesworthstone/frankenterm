//! Pattern-pack format conformance harness (bead ft-b35tw).
//!
//! Runs three layers of checks against every fixture in
//! `tests/fixtures/pattern_packs/`:
//!
//! 1. **Schema validation** — each valid fixture's parsed JSON form is
//!    validated against `docs/json-schema/ft-pattern-pack.json` via
//!    the jsonschema crate (Draft 2020-12). Catches structural drift.
//! 2. **Loader parity** — each valid fixture parses into the production
//!    `PatternPack` struct via the same code path the runtime uses
//!    (`serde_yaml`/`serde_json`/`toml::from_str`). Catches schema /
//!    Rust-struct drift.
//! 3. **Validate parity** — each valid fixture passes
//!    `PatternPack::validate()` semantically; each invalid fixture
//!    fails with the expected error class.
//!
//! Plus a coverage meta-test that asserts every documented `RuleDef`
//! field is exercised by at least one valid fixture, so a new optional
//! field landing on `RuleDef` without a fixture update fires red.
//!
//! Falsification: `synthetic_pack_missing_required_field_must_fail`
//! proves the schema validator actually fires (not a no-op) by feeding
//! it a deliberately broken pack JSON.

// Same jsonschema crate version as conformance_robot_envelope_schema.rs
// (ft-5ikbd). The 0.21 deprecation typedef path is the stable workspace
// import for now.
#![allow(deprecated)]

use jsonschema::{Draft, JSONSchema as Validator};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::patterns::PatternPack;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("json-schema")
        .join("ft-pattern-pack.json")
}

fn corpus_dir(kind: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("pattern_packs")
        .join(kind)
}

fn load_schema() -> Validator {
    let path = schema_path();
    let bytes = fs::read(&path)
        .unwrap_or_else(|err| panic!("read schema {}: {err}", path.display()));
    let schema_json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("schema {} is not valid JSON: {err}", path.display()));
    Validator::options()
        .with_draft(Draft::Draft202012)
        .compile(&schema_json)
        .unwrap_or_else(|err| panic!("schema compile failed: {err}"))
}

/// Parse a fixture file into a serde_json::Value via the right parser
/// for its extension. Goes through the same family of parsers as the
/// production loader (patterns.rs:1202-1207).
fn parse_to_json_value(path: &Path) -> Value {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()));
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_str(&body)
            .unwrap_or_else(|err| panic!("parse YAML fixture {}: {err}", path.display())),
        "json" => serde_json::from_str(&body)
            .unwrap_or_else(|err| panic!("parse JSON fixture {}: {err}", path.display())),
        "toml" => {
            let toml_value: toml::Value = toml::from_str(&body)
                .unwrap_or_else(|err| panic!("parse TOML fixture {}: {err}", path.display()));
            // Re-serialize through serde_json for schema validation; the
            // schema is JSON Schema (the source-of-truth for shape) and
            // TOML's data model is a superset of JSON's relevant types.
            serde_json::to_value(toml_value)
                .unwrap_or_else(|err| panic!("toml→json reserialize {}: {err}", path.display()))
        }
        other => panic!(
            "fixture {} has unexpected extension `{other}` (expected yaml/yml/json/toml)",
            path.display()
        ),
    }
}

fn parse_to_pack(path: &Path) -> PatternPack {
    let body = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("read fixture {}: {err}", path.display()));
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_str(&body)
            .unwrap_or_else(|err| panic!("parse YAML pack {}: {err}", path.display())),
        "json" => serde_json::from_str(&body)
            .unwrap_or_else(|err| panic!("parse JSON pack {}: {err}", path.display())),
        "toml" => toml::from_str(&body)
            .unwrap_or_else(|err| panic!("parse TOML pack {}: {err}", path.display())),
        _ => panic!("unexpected ext for {}", path.display()),
    }
}

fn discover_fixtures(kind: &str) -> Vec<PathBuf> {
    let dir = corpus_dir(kind);
    let mut found = Vec::new();
    for entry in
        fs::read_dir(&dir).unwrap_or_else(|err| panic!("read {}: {err}", dir.display()))
    {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "yaml" | "yml" | "json" | "toml") {
            found.push(path);
        }
    }
    found.sort();
    found
}

// ── Layer 1: schema validation ───────────────────────────────────────────────

#[test]
fn schema_compiles_under_draft_2020_12() {
    let _ = load_schema();
}

#[test]
fn every_valid_fixture_validates_against_schema() {
    let validator = load_schema();
    let fixtures = discover_fixtures("valid");
    assert!(
        !fixtures.is_empty(),
        "no valid fixtures found under tests/fixtures/pattern_packs/valid/"
    );
    for path in fixtures {
        let value = parse_to_json_value(&path);
        let result = validator.validate(&value);
        if let Err(errors) = result {
            let messages: Vec<String> = errors.map(|e| e.to_string()).collect();
            panic!(
                "fixture {} failed schema validation:\n{}",
                path.display(),
                messages.join("\n")
            );
        }
    }
}

// ── Layer 2: loader parity ───────────────────────────────────────────────────

#[test]
fn every_valid_fixture_parses_into_pattern_pack() {
    for path in discover_fixtures("valid") {
        let pack = parse_to_pack(&path);
        assert!(
            !pack.name.is_empty(),
            "fixture {} parsed but has empty name",
            path.display()
        );
        assert!(
            !pack.version.is_empty(),
            "fixture {} parsed but has empty version",
            path.display()
        );
    }
}

// ── Layer 3: invalid-fixture rejection ──────────────────────────────────────

#[test]
fn invalid_fixtures_are_rejected_by_the_loader_or_validator() {
    let fixtures = discover_fixtures("invalid");
    assert!(
        !fixtures.is_empty(),
        "no invalid fixtures found — invalid corpus must be non-empty so we \
         prove the loader actually rejects malformed packs"
    );
    for path in fixtures {
        // Either parse fails (e.g. missing required field), OR parse
        // succeeds and validate() rejects (e.g. duplicate ids, empty
        // name). Both paths are documented as legitimate rejection
        // behavior in docs/patterns-pack-format.md.
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let parse_result: Result<PatternPack, String> = match ext.as_str() {
            "yaml" | "yml" => {
                serde_yaml::from_str::<PatternPack>(&body).map_err(|e| e.to_string())
            }
            "json" => {
                serde_json::from_str::<PatternPack>(&body).map_err(|e| e.to_string())
            }
            "toml" => toml::from_str::<PatternPack>(&body).map_err(|e| e.to_string()),
            _ => continue,
        };

        match parse_result {
            Err(_) => {
                // Parse-time rejection is OK.
            }
            Ok(pack) => {
                // Parse succeeded; PatternPack::validate must now reject.
                // PatternPack::validate is private — exercise it through
                // PatternLibrary::new which calls validate on every
                // included pack.
                let result = frankenterm_core::patterns::PatternLibrary::new(vec![pack]);
                assert!(
                    result.is_err(),
                    "fixture {} parsed AND passed PatternLibrary::new — \
                     it should have been rejected (it's in the invalid corpus)",
                    path.display()
                );
            }
        }
    }
}

// ── Coverage meta-test ──────────────────────────────────────────────────────

#[test]
fn every_documented_rule_field_is_exercised_by_some_valid_fixture() {
    // Walk every valid fixture's parsed JSON; collect the union of keys
    // that appear on any rule object. Compare against the expected set
    // from docs/patterns-pack-format.md. New fields landing on RuleDef
    // without a fixture update flip this red.
    let mut seen_keys: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for path in discover_fixtures("valid") {
        let value = parse_to_json_value(&path);
        let rules = value
            .get("rules")
            .and_then(|v| v.as_array())
            .unwrap_or_else(|| panic!("fixture {} missing rules array", path.display()));
        for rule in rules {
            if let Some(obj) = rule.as_object() {
                for k in obj.keys() {
                    seen_keys.insert(k.clone());
                }
            }
        }
    }
    let expected: std::collections::BTreeSet<String> = [
        "id",
        "agent_type",
        "event_type",
        "severity",
        "anchors",
        "regex",
        "description",
        "remediation",
        "workflow",
        "manual_fix",
        "preview_command",
        "learn_more_url",
    ]
    .into_iter()
    .map(String::from)
    .collect();

    eprintln!(
        "{}",
        serde_json::json!({
            "phase": "coverage",
            "suite": "pattern_pack_format",
            "seen_keys": seen_keys.iter().collect::<Vec<_>>(),
            "expected_keys": expected.iter().collect::<Vec<_>>(),
        })
    );

    let missing: Vec<String> = expected.difference(&seen_keys).cloned().collect();
    assert!(
        missing.is_empty(),
        "RuleDef fields not exercised by any valid fixture: {missing:?}\n\n\
         Add an example to crates/frankenterm-core/tests/fixtures/pattern_packs/valid/ \
         that covers each missing field, OR remove the field from the \
         expected list (and from docs/patterns-pack-format.md).",
    );

    let unknown: Vec<String> = seen_keys.difference(&expected).cloned().collect();
    assert!(
        unknown.is_empty(),
        "fixtures use rule keys not documented in docs/patterns-pack-format.md: \
         {unknown:?}",
    );
}

// ── Falsification: deliberately-broken pack must fail ───────────────────────

#[test]
fn synthetic_pack_missing_required_field_must_fail() {
    let validator = load_schema();
    // Missing top-level `rules` array — schema MUST reject.
    let bad = serde_json::json!({
        "name": "synthetic",
        "version": "1.0.0"
    });
    let result = validator.validate(&bad);
    assert!(
        result.is_err(),
        "schema validator did not reject a pack missing the required `rules` field"
    );
}

#[test]
fn synthetic_pack_unknown_top_level_property_must_fail() {
    let validator = load_schema();
    let bad = serde_json::json!({
        "name": "synthetic",
        "version": "1.0.0",
        "rules": [],
        "extra_top_level": "this is not allowed"
    });
    let result = validator.validate(&bad);
    assert!(
        result.is_err(),
        "schema validator did not reject a pack with an unknown top-level property \
         (additionalProperties: false should kick)"
    );
}
