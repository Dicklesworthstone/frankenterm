//! Runtime JSON-Schema validator for robot-mode response envelopes (ft-5ikbd).
//!
//! Enforces MCP-V1-001 and MCP-V1-005 — see docs/mcp-api-spec-coverage.md.
//!
//! `docs/mcp-api-spec.md` declares the central MUST clause:
//!
//! > `data` MUST match the corresponding robot JSON schema under
//! > `docs/json-schema/`.
//!
//! Pre-fix, no runtime test enforced it: `tests/schema_golden.rs` only
//! checked that the schemas were well-formed JSON, and the
//! `tests/golden_robot_envelope/*.json` fixtures pinned envelope
//! *examples* without ever validating them against the schema. A Rust
//! struct that gained a field without a matching schema update would
//! land green; downstream JSON consumers (TypeScript clients, MCP
//! agents) would silently break.
//!
//! This test loads `docs/json-schema/wa-robot-envelope.json` and
//! validates each `success_envelope` from the golden fixtures against
//! it via the `jsonschema` crate (Draft 2020-12, matching the
//! `$schema` declared in the schema files).
//!
//! # Coverage
//!
//! Every fixture in `tests/golden_robot_envelope/` whose top-level
//! shape carries a `success_envelope` field is validated. The fixture
//! list itself is discovered from disk so adding a new fixture
//! automatically extends coverage.
//!
//! # Falsification
//!
//! `synthetic_envelope_missing_required_field_must_fail` proves the
//! validator actually fires (not a no-op) by feeding it a deliberately
//! broken envelope and asserting validation fails.

// jsonschema 0.21 deprecated `JSONSchema` → `Validator` and
// `compile()` → `build()`, but the Draft::Draft202012 + builder/options
// API path is not yet stable across the 0.18→0.22 series. Use the
// stable typedef + #[allow] for now; ft will follow upstream stabilization
// when jsonschema reaches 1.0.
#[allow(deprecated)]
use jsonschema::{Draft, JSONSchema as Validator};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn robot_envelope_schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("json-schema")
        .join("wa-robot-envelope.json")
}

fn mcp_envelope_schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("json-schema")
        .join("wa-mcp-envelope.json")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_robot_envelope")
}

#[allow(deprecated)]
fn load_envelope_schema() -> Validator {
    load_schema(&robot_envelope_schema_path())
}

#[allow(deprecated)]
fn load_mcp_envelope_schema() -> Validator {
    load_schema(&mcp_envelope_schema_path())
}

#[allow(deprecated)]
fn load_schema(path: &Path) -> Validator {
    let bytes = fs::read(&path).unwrap_or_else(|err| {
        panic!(
            "failed to read envelope schema at {}: {err}",
            path.display()
        )
    });
    let schema_json: Value = serde_json::from_slice(&bytes).unwrap_or_else(|err| {
        panic!(
            "envelope schema is not valid JSON ({}): {err}",
            path.display()
        )
    });
    Validator::options()
        .with_draft(Draft::Draft202012)
        .compile(&schema_json)
        .unwrap_or_else(|err| panic!("envelope schema compile failed: {err}"))
}

/// Discover all `wa_*.json` fixtures in the goldens directory. The
/// `wa_` prefix filters out the legacy non-envelope payload files
/// (`scalar_payload.json`, `nested_timestamp_payload.json`, etc.) that
/// pin payload-shape goldens, not full envelopes.
fn discover_fixtures() -> Vec<(String, PathBuf)> {
    let dir = fixtures_dir();
    let mut found: Vec<(String, PathBuf)> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read goldens dir {}: {err}", dir.display()))
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("wa_") && name.ends_with(".json") {
                Some((name, path))
            } else {
                None
            }
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Load the `success_envelope` field from a fixture file. Returns
/// `None` for fixtures that don't contain one (e.g., the
/// `wa_tx_toon_conformance.json` fixture pins a TOON conformance
/// matrix rather than a single envelope).
fn load_success_envelope(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    v.as_object()?.get("success_envelope").cloned()
}

#[test]
fn every_golden_envelope_validates_against_schema() {
    let schema = load_envelope_schema();
    let fixtures = discover_fixtures();
    assert!(
        !fixtures.is_empty(),
        "no wa_* fixtures discovered under {}",
        fixtures_dir().display()
    );

    let mut validated = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (name, path) in &fixtures {
        let Some(envelope) = load_success_envelope(path) else {
            skipped += 1;
            eprintln!(
                "{{\"phase\":\"skip\",\"fixture\":{name:?},\"reason\":\"no success_envelope\"}}"
            );
            continue;
        };

        let result = schema.validate(&envelope);
        match result {
            Ok(()) => {
                validated += 1;
                eprintln!(
                    "{{\"phase\":\"pass\",\"fixture\":{name:?},\"path\":{:?}}}",
                    path.to_string_lossy()
                );
            }
            Err(errors) => {
                let collected: Vec<String> = errors
                    .map(|e| format!("    - {} (instance: {})", e, e.instance_path))
                    .collect();
                failures.push(format!(
                    "{name}:\n{}",
                    if collected.is_empty() {
                        "    (validator returned Err with no items)".to_string()
                    } else {
                        collected.join("\n")
                    }
                ));
                eprintln!("{{\"phase\":\"fail\",\"fixture\":{name:?}}}");
            }
        }
    }

    eprintln!(
        "{{\"phase\":\"summary\",\"validated\":{validated},\"skipped\":{skipped},\"failed\":{}}}",
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "{} envelope fixtures failed schema validation:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert!(
        validated >= 10,
        "expected ≥10 envelopes validated; got {validated}. Did the fixtures move?"
    );
}

/// Falsification gate. If this test does NOT fail, the validator is
/// a no-op and the surrounding pass-suite proves nothing.
#[test]
fn synthetic_envelope_missing_required_field_must_fail() {
    let schema = load_envelope_schema();

    // Missing `now` — schema says it's required.
    let broken = serde_json::json!({
        "ok": true,
        "data": { "answer": 42 },
        "elapsed_ms": 5,
        "version": "0.1.0",
        // "now" omitted intentionally
    });

    let result = schema.validate(&broken);
    assert!(
        result.is_err(),
        "validator MUST reject envelope missing required `now` field — \
         if this test passes, the schema validator is a no-op and the \
         pass-suite proves nothing about real envelopes"
    );
}

/// Negative case: malformed `error_code` (must match `^robot\.[a-z_]+$`).
#[test]
fn synthetic_envelope_with_malformed_error_code_must_fail() {
    let schema = load_envelope_schema();

    let broken = serde_json::json!({
        "ok": false,
        "error": "boom",
        "error_code": "NotARobotCode",       // missing 'robot.' prefix
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
    });

    let result = schema.validate(&broken);
    assert!(
        result.is_err(),
        "validator MUST reject error_code that doesn't match ^robot\\.[a-z_]+$"
    );
}

#[test]
fn synthetic_robot_error_envelope_with_hint_validates() {
    let schema = load_envelope_schema();

    let envelope = serde_json::json!({
        "ok": false,
        "error": "Pane 99 not found",
        "error_code": "robot.pane_not_found",
        "hint": "Use ft robot state to list available panes.",
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
    });

    schema
        .validate(&envelope)
        .unwrap_or_else(|errors| panic_validation_errors("robot error envelope", errors));
}

#[test]
fn synthetic_robot_error_envelope_without_hint_validates() {
    let schema = load_envelope_schema();

    let envelope = serde_json::json!({
        "ok": false,
        "error": "Storage unavailable",
        "error_code": "robot.storage_error",
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
    });

    schema
        .validate(&envelope)
        .unwrap_or_else(|errors| panic_validation_errors("robot error envelope", errors));
}

/// Negative case: ok=true but no `data` field. The schema's
/// conditional clause (if ok==true then data is required) must fire.
#[test]
fn synthetic_envelope_ok_true_without_data_must_fail() {
    let schema = load_envelope_schema();

    let broken = serde_json::json!({
        "ok": true,
        // "data" omitted intentionally
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
    });

    let result = schema.validate(&broken);
    assert!(
        result.is_err(),
        "validator MUST reject ok=true envelope missing `data` (schema's \
         if/then conditional clause)"
    );
}

#[allow(deprecated)]
fn panic_validation_errors(label: &str, errors: jsonschema::ErrorIterator<'_>) {
    let collected: Vec<String> = errors
        .map(|e| format!("    - {} (instance: {})", e, e.instance_path))
        .collect();
    panic!(
        "{label} failed schema validation:\n{}",
        if collected.is_empty() {
            "    (validator returned Err with no items)".to_string()
        } else {
            collected.join("\n")
        }
    );
}

#[test]
fn synthetic_mcp_success_envelope_validates_against_schema() {
    let schema = load_mcp_envelope_schema();

    let envelope = serde_json::json!({
        "ok": true,
        "data": { "tool": "wa.search", "matches": [] },
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
        "mcp_version": "v1",
    });

    schema
        .validate(&envelope)
        .unwrap_or_else(|errors| panic_validation_errors("MCP success envelope", errors));
}

#[test]
fn synthetic_mcp_error_envelope_with_hint_validates_against_schema() {
    let schema = load_mcp_envelope_schema();

    let envelope = serde_json::json!({
        "ok": false,
        "error": "invalid arguments",
        "error_code": "FT-MCP-0001",
        "hint": "Pass an object with a pane_id field.",
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
        "mcp_version": "v1",
    });

    schema
        .validate(&envelope)
        .unwrap_or_else(|errors| panic_validation_errors("MCP error envelope", errors));
}

#[test]
fn synthetic_mcp_error_envelope_without_hint_validates_against_schema() {
    let schema = load_mcp_envelope_schema();

    let envelope = serde_json::json!({
        "ok": false,
        "error": "Storage unavailable",
        "error_code": "FT-MCP-0005",
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
        "mcp_version": "v1",
    });

    schema
        .validate(&envelope)
        .unwrap_or_else(|errors| panic_validation_errors("MCP error envelope", errors));
}

#[test]
fn synthetic_mcp_error_envelope_missing_mcp_version_must_fail() {
    let schema = load_mcp_envelope_schema();

    let broken = serde_json::json!({
        "ok": false,
        "error": "invalid arguments",
        "error_code": "FT-MCP-0001",
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
    });

    let result = schema.validate(&broken);
    assert!(
        result.is_err(),
        "validator MUST reject MCP envelope missing required `mcp_version` field"
    );
}

#[test]
fn synthetic_mcp_error_envelope_with_robot_error_code_must_fail() {
    let schema = load_mcp_envelope_schema();

    let broken = serde_json::json!({
        "ok": false,
        "error": "invalid arguments",
        "error_code": "robot.invalid_args",
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1_700_000_000_000_u64,
        "mcp_version": "v1",
    });

    let result = schema.validate(&broken);
    assert!(
        result.is_err(),
        "validator MUST reject robot.* error codes in MCP envelopes"
    );
}

/// Coverage assertion: prints the fixture inventory so a CI reviewer
/// can audit what's covered without re-deriving from the test output.
#[test]
fn fixture_inventory_is_visible() {
    let fixtures = discover_fixtures();
    eprintln!("--- ft-5ikbd: robot envelope fixture inventory ---");
    for (name, path) in &fixtures {
        let with_envelope = load_success_envelope(path).is_some();
        eprintln!(
            "  {:<40} {}",
            name,
            if with_envelope {
                "[envelope]"
            } else {
                "[other]"
            }
        );
    }
    eprintln!("--- {} fixtures total ---", fixtures.len());
    assert!(
        fixtures.len() >= 14,
        "expected ≥14 fixtures; got {}",
        fixtures.len()
    );
}
