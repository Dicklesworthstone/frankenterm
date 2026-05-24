//! Runtime conformance test for the documented `ft.toml` surface (ft-2sumi).
//!
//! Pre-fix the README at line 632 documents 53+ TOML config keys but no
//! test enforced any of three drift-prevention contracts:
//!
//!   1. The README's canonical config block actually parses through
//!      `Config::from_toml`. (default serde drops unknown fields
//!      silently — a rename in code without a README update lands
//!      green; an operator copying the README into ft.toml would see
//!      no error and ft would fall back to defaults.)
//!   2. The documented sub-section names round-trip through
//!      `Config::to_toml` (proving the documented keys actually map
//!      to `Config` fields, not just to silently-dropped unknowns).
//!   3. The fixture matches the README byte-for-byte (so updating
//!      the README and the fixture move together in one PR diff).
//!
//! And one more positive contract:
//!
//!   4. A JSON Schema for the documented surface
//!      (`docs/json-schema/ft-config.json`) accepts the canonical
//!      fixture and rejects deliberately-malformed inputs (bogus
//!      `log_level` enum, negative `retention_days`).
//!
//! The schema is intentionally permissive at sub-section level
//! (`additionalProperties: true`) — internal-only sub-keys (sync,
//! distributed, ipc, native, metrics, snapshots, search, tuning,
//! …) validate without explicit modeling. The schema's job is to
//! enforce the README-documented shape, not to gate every internal
//! tuning knob.

#![allow(deprecated)]

use jsonschema::{Draft, JSONSchema};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("ft_config_readme.toml")
}

fn schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("json-schema")
        .join("ft-config.json")
}

fn readme_path() -> PathBuf {
    workspace_root().join("README.md")
}

fn load_fixture_toml() -> String {
    fs::read_to_string(fixture_path()).unwrap_or_else(|err| panic!("failed to read fixture: {err}"))
}

fn compile_config_schema() -> JSONSchema {
    let bytes =
        fs::read(schema_path()).unwrap_or_else(|err| panic!("failed to read schema: {err}"));
    let v: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("schema is not valid JSON: {err}"));
    JSONSchema::options()
        .with_draft(Draft::Draft202012)
        .compile(&v)
        .unwrap_or_else(|err| panic!("schema compile failed: {err}"))
}

fn fixture_as_json() -> Value {
    let toml_str = load_fixture_toml();
    let toml_value: toml::Value =
        toml::from_str(&toml_str).unwrap_or_else(|err| panic!("fixture is not valid TOML: {err}"));
    serde_json::to_value(toml_value)
        .unwrap_or_else(|err| panic!("TOML→JSON conversion failed: {err}"))
}

/// Extract the README's canonical TOML block — the lines between
/// `## Configuration` and `### Environment Variables` that fall
/// inside a ```toml fence.
fn extract_readme_toml() -> String {
    let readme = fs::read_to_string(readme_path())
        .unwrap_or_else(|err| panic!("failed to read README: {err}"));

    let mut in_section = false;
    let mut in_fence = false;
    let mut out: Vec<&str> = Vec::new();

    for line in readme.lines() {
        if line == "## Configuration" {
            in_section = true;
            continue;
        }
        if line == "### Environment Variables" {
            break;
        }
        if !in_section {
            continue;
        }

        if line == "```toml" {
            in_fence = true;
            continue;
        }
        if line == "```" {
            in_fence = false;
            continue;
        }
        if in_fence {
            out.push(line);
        }
    }

    let mut joined = out.join("\n");
    joined.push('\n');
    joined
}

/// Sub-sections the README documents AND the production `Config`
/// struct round-trips. These are the sections an operator can rely on:
/// putting them in `ft.toml` will actually shape ft's behavior.
const README_DOCUMENTED_SECTIONS: &[&str] = &[
    "general",
    "ingest",
    "storage",
    "gc",
    "vendored",
    "backup",
    "patterns",
    "workflows",
    "safety",
    "agent_detection",
];

/// Sub-sections the README documents but production `Config` silently
/// drops because there's no matching field on the struct. Operators
/// putting these in `ft.toml` see no error and ft falls back to
/// internal defaults — exactly the silent-drop hazard ft-2sumi was
/// filed to detect.
///
/// Each entry here is a real drift bug; the trailing bead ID is
/// where the README↔code reconciliation is tracked.
const README_DOCUMENTED_BUT_UNREACHABLE: &[(&str, &str)] = &[];

/// Step 1: The README's canonical TOML block must parse through the
/// production loader without erroring. If serde's deserializer
/// hard-fails on a documented key, the README is lying about that
/// key's name or shape.
#[test]
fn readme_canonical_toml_parses_through_production_loader() {
    let toml_str = load_fixture_toml();
    let result = frankenterm_core::config::Config::from_toml(&toml_str);
    assert!(
        result.is_ok(),
        "Config::from_toml rejected the README's canonical config block:\n  {:?}",
        result.err()
    );
}

/// Step 1b: The README's canonical TOML block must also pass the
/// semantic validator used by `Config::load_with_overrides`. Parsing
/// alone is not enough: serde can deserialize a shape that the
/// runtime later rejects (invalid pane filters, priority rules,
/// storage safety, etc.).
#[test]
fn readme_canonical_toml_validates_through_production_loader() {
    let toml_str = load_fixture_toml();
    let mut cfg = frankenterm_core::config::Config::from_toml(&toml_str)
        .expect("README canonical config must parse");
    cfg.normalize_paths();
    cfg.validate()
        .expect("README canonical config must pass semantic validation");
}

/// Step 2: Round-trip parse → re-serialize. The re-serialized form must
/// contain every README-documented top-level section.
///
/// If a documented section is silently dropped (because it was
/// renamed in code but not in the README), the round-tripped TOML
/// will lack that header. This catches the exact silent-drop path
/// that motivates the bead — pre-fix, an operator's `ft.toml`
/// would parse without error but ft would fall back to defaults.
#[test]
fn round_trip_preserves_every_readme_documented_section() {
    let toml_str = load_fixture_toml();
    let cfg = frankenterm_core::config::Config::from_toml(&toml_str)
        .expect("README canonical config must parse");
    let reserialized = cfg
        .to_toml()
        .expect("Config::to_toml must succeed on a default-shaped config");

    let mut missing: Vec<&str> = Vec::new();
    for section in README_DOCUMENTED_SECTIONS {
        let header_a = format!("[{section}]");
        let header_b = format!("[{section}.");
        if !reserialized.contains(&header_a) && !reserialized.contains(&header_b) {
            missing.push(section);
        }
    }
    assert!(
        missing.is_empty(),
        "README documents top-level sections that did NOT survive parse → \
         re-serialize through Config — production loader is silently \
         dropping them.\nMissing: {missing:?}\n\
         Re-serialized config (first 500 chars):\n{}",
        reserialized.chars().take(500).collect::<String>()
    );
}

/// Step 3: Parity gate: the fixture file must match the README's
/// canonical TOML block byte-for-byte. Updating the README without
/// updating the fixture (or vice versa) fails this test, forcing
/// both edits into the same PR diff.
#[test]
fn fixture_matches_readme_canonical_toml_block() {
    let from_readme = extract_readme_toml();
    let from_fixture = load_fixture_toml();

    if from_readme != from_fixture {
        // Write the actual readme block to a sibling file so the
        // failure includes a usable diff.
        let actual = fixture_path().with_extension("readme.actual");
        let _ = fs::write(&actual, from_readme.as_bytes());
        panic!(
            "fixture diverged from README.\n  fixture: {}\n  readme block extracted: {}\n  diff: \
             diff -u {} {}",
            fixture_path().display(),
            actual.display(),
            fixture_path().display(),
            actual.display(),
        );
    }
}

/// 4a) The schema must accept the canonical fixture.
#[test]
fn schema_accepts_canonical_fixture() {
    let schema = compile_config_schema();
    let json = fixture_as_json();
    let result = schema.validate(&json);
    if let Err(errors) = result {
        let lines: Vec<String> = errors
            .map(|e| format!("    - {} (instance: {})", e, e.instance_path))
            .collect();
        panic!(
            "schema rejected the README's canonical fixture:\n{}",
            lines.join("\n")
        );
    }
}

/// 4b) Falsification: a config with a bogus `log_level` enum value
/// MUST be rejected. If this passes, the schema's enum constraint
/// isn't firing and the pass-suite proves nothing.
#[test]
fn schema_rejects_invalid_log_level() {
    let schema = compile_config_schema();
    let bad = serde_json::json!({
        "general": { "log_level": "loud" },
    });
    let result = schema.validate(&bad);
    assert!(
        result.is_err(),
        "schema MUST reject general.log_level=\"loud\" — if this passes, \
         the enum constraint is a no-op"
    );
}

/// 4c) Falsification: a negative `retention_days` MUST be rejected
/// (the schema declares `minimum: 0`).
#[test]
fn schema_rejects_negative_retention_days() {
    let schema = compile_config_schema();
    let bad = serde_json::json!({
        "storage": { "retention_days": -1 },
    });
    let result = schema.validate(&bad);
    assert!(
        result.is_err(),
        "schema MUST reject storage.retention_days < 0 — if this passes, \
         the minimum constraint is a no-op"
    );
}

/// 4c.1) Falsification: `gc.vacuum_threshold` is a ratio and must
/// stay inside the documented [0, 1] range.
#[test]
fn schema_rejects_out_of_range_vacuum_threshold() {
    let schema = compile_config_schema();
    for (label, threshold) in [("negative", -0.01), ("above_one", 1.01)] {
        let bad = serde_json::json!({
            "gc": { "vacuum_threshold": threshold },
        });
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject {label} gc.vacuum_threshold values"
        );
    }
}

/// 4c.2) Falsification: documented counters and millisecond knobs
/// with `minimum: 0` must reject negative values across sections.
#[test]
fn schema_rejects_negative_documented_nonnegative_fields() {
    let schema = compile_config_schema();
    for (surface, bad) in [
        (
            "ingest.poll_interval_ms",
            serde_json::json!({
                "ingest": { "poll_interval_ms": -1 },
            }),
        ),
        (
            "gc.interval_seconds",
            serde_json::json!({
                "gc": { "interval_seconds": -1 },
            }),
        ),
        (
            "safety.rate_limit_global",
            serde_json::json!({
                "safety": { "rate_limit_global": -1 },
            }),
        ),
        (
            "agent_detection.idle_silence_ms",
            serde_json::json!({
                "agent_detection": { "idle_silence_ms": -1 },
            }),
        ),
    ] {
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject negative documented nonnegative field {surface}"
        );
    }
}

/// 4d) Falsification: `vendored.sharding.assignment.strategy` is an
/// enum; an unknown strategy MUST be rejected.
#[test]
fn schema_rejects_unknown_sharding_strategy() {
    let schema = compile_config_schema();
    let bad = serde_json::json!({
        "vendored": {
            "sharding": {
                "enabled": true,
                "assignment": { "strategy": "magic" }
            }
        }
    });
    let result = schema.validate(&bad);
    assert!(
        result.is_err(),
        "schema MUST reject unknown sharding strategy — if this passes, \
         the enum constraint is a no-op"
    );
}

/// 4e) Falsification: pane filter rule IDs are required by the
/// production `PaneFilterRule` validator and must not be empty.
#[test]
fn schema_rejects_empty_pane_filter_rule_id() {
    let schema = compile_config_schema();
    for (label, id) in [("empty", ""), ("blank", " \t")] {
        let bad = serde_json::json!({
            "ingest": {
                "panes": {
                    "include": [
                        { "id": id, "title": "codex" }
                    ]
                }
            }
        });
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject ingest.panes.include entries with {label} IDs"
        );
    }
}

/// 4f) Falsification: pane filters and priority overrides must carry
/// at least one matcher field, not just an ID.
#[test]
fn schema_rejects_matcherless_pane_rules() {
    let schema = compile_config_schema();
    for (surface, bad) in [
        (
            "ingest.panes.include",
            serde_json::json!({
                "ingest": {
                    "panes": {
                        "include": [
                            { "id": "missing_matcher" }
                        ]
                    }
                }
            }),
        ),
        (
            "ingest.priorities.rules",
            serde_json::json!({
                "ingest": {
                    "priorities": {
                        "rules": [
                            { "id": "missing_matcher", "priority": 10 }
                        ]
                    }
                }
            }),
        ),
    ] {
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject matcher-less {surface} entries"
        );
    }
}

/// 4g) Falsification: matcher strings must be non-empty and non-blank,
/// because an empty title substring would otherwise match every pane.
#[test]
fn schema_rejects_empty_or_blank_pane_matcher_values() {
    let schema = compile_config_schema();
    for (surface, bad) in [
        (
            "ingest.panes.include.title",
            serde_json::json!({
                "ingest": {
                    "panes": {
                        "include": [
                            { "id": "empty_title", "title": "" }
                        ]
                    }
                }
            }),
        ),
        (
            "ingest.panes.include.domain",
            serde_json::json!({
                "ingest": {
                    "panes": {
                        "include": [
                            { "id": "blank_domain", "domain": " \t" }
                        ]
                    }
                }
            }),
        ),
        (
            "ingest.priorities.rules.cwd",
            serde_json::json!({
                "ingest": {
                    "priorities": {
                        "rules": [
                            { "id": "empty_cwd", "priority": 10, "cwd": "" }
                        ]
                    }
                }
            }),
        ),
        (
            "ingest.priorities.rules.title",
            serde_json::json!({
                "ingest": {
                    "priorities": {
                        "rules": [
                            { "id": "blank_title", "priority": 10, "title": "\n" }
                        ]
                    }
                }
            }),
        ),
    ] {
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject empty or blank matcher string at {surface}"
        );
    }
}

/// 4h) Falsification: documented free-form strings that name schedules,
/// pattern packs, or workflows must not be blank.
#[test]
fn schema_rejects_blank_documented_string_values() {
    let schema = compile_config_schema();
    for (surface, bad) in [
        (
            "backup.scheduled.schedule",
            serde_json::json!({
                "backup": {
                    "scheduled": {
                        "schedule": " \t"
                    }
                }
            }),
        ),
        (
            "patterns.packs[]",
            serde_json::json!({
                "patterns": {
                    "packs": ["builtin:core", ""]
                }
            }),
        ),
        (
            "workflows.enabled[]",
            serde_json::json!({
                "workflows": {
                    "enabled": ["handle_compaction", "\n"]
                }
            }),
        ),
    ] {
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject blank documented string at {surface}"
        );
    }
}

/// 4h.1) Falsification: documented path-like strings must not be
/// whitespace-only. A blank path in ft.toml is never a usable location.
#[test]
fn schema_rejects_blank_documented_path_values() {
    let schema = compile_config_schema();
    for (surface, bad) in [
        (
            "general.data_dir",
            serde_json::json!({
                "general": {
                    "data_dir": " \t"
                }
            }),
        ),
        (
            "vendored.mux_socket_path",
            serde_json::json!({
                "vendored": {
                    "mux_socket_path": "\n"
                }
            }),
        ),
        (
            "vendored.mux_pool.compression",
            serde_json::json!({
                "vendored": {
                    "mux_pool": {
                        "compression": " "
                    }
                }
            }),
        ),
        (
            "vendored.sharding.socket_paths[]",
            serde_json::json!({
                "vendored": {
                    "sharding": {
                        "socket_paths": ["/tmp/ft-shard-0.sock", " \t"]
                    }
                }
            }),
        ),
        (
            "backup.scheduled.destination",
            serde_json::json!({
                "backup": {
                    "scheduled": {
                        "destination": " \t"
                    }
                }
            }),
        ),
    ] {
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject blank documented path-like string at {surface}"
        );
    }
}

/// 4i) Falsification: documented bounded-size fields that must be at
/// least one should reject zero. These mirror production semantic
/// validators for queue sizes and concurrency.
#[test]
fn schema_rejects_zero_for_documented_minimum_one_fields() {
    let schema = compile_config_schema();
    for (surface, bad) in [
        (
            "storage.writer_queue_size",
            serde_json::json!({
                "storage": {
                    "writer_queue_size": 0
                }
            }),
        ),
        (
            "workflows.max_concurrent",
            serde_json::json!({
                "workflows": {
                    "max_concurrent": 0
                }
            }),
        ),
        (
            "vendored.mux_pool.max_connections",
            serde_json::json!({
                "vendored": {
                    "mux_pool": {
                        "max_connections": 0
                    }
                }
            }),
        ),
        (
            "vendored.mux_pool.pipeline_depth",
            serde_json::json!({
                "vendored": {
                    "mux_pool": {
                        "pipeline_depth": 0
                    }
                }
            }),
        ),
    ] {
        assert!(
            schema.validate(&bad).is_err(),
            "schema MUST reject zero for documented minimum-one field {surface}"
        );
    }
}

/// Pin the documented-but-unreachable list. If the README↔code
/// reconciliation lands and the unreachable section starts round-
/// tripping, this test fails and the maintainer must move the entry
/// from `README_DOCUMENTED_BUT_UNREACHABLE` to `README_DOCUMENTED_SECTIONS`.
///
/// (XFAIL pattern from /testing-conformance-harnesses: known
/// divergences are tracked, not skipped.)
#[test]
fn unreachable_documented_sections_remain_unreachable() {
    let toml_str = load_fixture_toml();
    let cfg = frankenterm_core::config::Config::from_toml(&toml_str)
        .expect("README canonical config must parse");
    let reserialized = cfg.to_toml().expect("Config::to_toml must succeed");

    let mut surprise_passes: Vec<&str> = Vec::new();
    for (section, bead) in README_DOCUMENTED_BUT_UNREACHABLE {
        let header_a = format!("[{section}]");
        let header_b = format!("[{section}.");
        if reserialized.contains(&header_a) || reserialized.contains(&header_b) {
            surprise_passes.push(*bead);
            eprintln!(
                "[XFAIL→PASS] section [{section}] now round-trips through Config; \
                 move it from README_DOCUMENTED_BUT_UNREACHABLE to \
                 README_DOCUMENTED_SECTIONS and close {bead}."
            );
        }
    }
    assert!(
        surprise_passes.is_empty(),
        "{} previously-unreachable section(s) now round-trip — drift was \
         resolved. Update the test arrays and close the linked beads: {:?}",
        surprise_passes.len(),
        surprise_passes,
    );
}

/// Coverage assertion: prints the README-documented section list +
/// where each one was found in the round-tripped output, so a CI
/// reviewer can audit coverage from stdout without re-deriving from
/// the test results.
#[test]
fn coverage_inventory_is_visible() {
    let toml_str = load_fixture_toml();
    let cfg = frankenterm_core::config::Config::from_toml(&toml_str)
        .expect("README canonical config must parse");
    let reserialized = cfg.to_toml().expect("Config::to_toml must succeed");

    eprintln!("--- ft-2sumi: README config inventory ---");
    for section in README_DOCUMENTED_SECTIONS {
        let header_a = format!("[{section}]");
        let header_b = format!("[{section}.");
        let present = reserialized.contains(&header_a) || reserialized.contains(&header_b);
        eprintln!(
            "  [{section:<18}] {}",
            if present { "round-trip" } else { "MISSING" }
        );
    }
    eprintln!("--- known unreachable (drift) ---");
    for (section, bead) in README_DOCUMENTED_BUT_UNREACHABLE {
        eprintln!("  [{section:<18}] XFAIL → tracked in {bead}");
    }
    eprintln!(
        "--- {} sections + {} XFAIL ---",
        README_DOCUMENTED_SECTIONS.len(),
        README_DOCUMENTED_BUT_UNREACHABLE.len()
    );

    assert!(README_DOCUMENTED_SECTIONS.len() >= 9);
}
