//! Golden tests for JSON Schema validation and docs generation determinism.
//!
//! Validates that:
//! 1. All hand-authored JSON Schema files are valid JSON
//! 2. Schema files have required JSON Schema fields
//! 3. SchemaRegistry covers all on-disk schemas (no orphans)
//! 4. Every registry endpoint has a corresponding schema file
//! 5. Docs generation is deterministic (same input → same output)
//! 6. Generated reference has expected structural elements
//!
//! # Related Beads
//!
//! - wa-upg.10.5: Tests: schema validation + docs generation golden tests
//! - wa-upg.10.1: Schema-driven API strategy
//! - wa-upg.10.2: Schema-driven docs generator

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use wa_core::api_schema::SchemaRegistry;
use wa_core::docs_gen::{
    DocGenConfig, EndpointCategory, categorize_endpoint, generate_endpoint_summary,
    generate_reference, parse_schema,
};

/// Workspace root: two levels up from crate manifest dir.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

/// Path to docs/json-schema/ directory.
fn schema_dir() -> PathBuf {
    workspace_root().join("docs").join("json-schema")
}

/// Load all .json files from docs/json-schema/.
fn load_all_schemas() -> Vec<(String, Value)> {
    let dir = schema_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut schemas: Vec<(String, Value)> = fs::read_dir(&dir)
        .expect("read schema dir")
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                let content = fs::read_to_string(&path).ok()?;
                let value: Value = serde_json::from_str(&content).ok()?;
                Some((name, value))
            } else {
                None
            }
        })
        .collect();

    schemas.sort_by(|a, b| a.0.cmp(&b.0));
    schemas
}

// ─────────────────────────────────────────────────────────────────────
// Schema file validation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn all_schema_files_are_valid_json() {
    let dir = schema_dir();
    if !dir.exists() {
        return; // Skip if schemas dir doesn't exist (CI without full checkout)
    }

    let entries: Vec<_> = fs::read_dir(&dir)
        .expect("read schema dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "json")
                .unwrap_or(false)
        })
        .collect();

    assert!(!entries.is_empty(), "schema dir should not be empty");

    for entry in entries {
        let path = entry.path();
        let content = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("Failed to read {}: {e}", path.display());
        });
        let _: Value = serde_json::from_str(&content).unwrap_or_else(|e| {
            panic!(
                "Invalid JSON in {}: {e}",
                entry.file_name().to_string_lossy()
            );
        });
    }
}

#[test]
fn schema_files_have_required_fields() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        // Every schema should have a title
        assert!(
            schema.get("title").and_then(Value::as_str).is_some(),
            "{name} missing 'title'"
        );

        // Every schema should have a description
        assert!(
            schema.get("description").and_then(Value::as_str).is_some(),
            "{name} missing 'description'"
        );

        // Data schemas (not envelope) should have type: "object"
        if name != "wa-robot-envelope.json" {
            // Most schemas should have a type field
            let has_type = schema.get("type").is_some();
            assert!(has_type, "{name} missing 'type'");
        }
    }
}

#[test]
fn schema_files_use_json_schema_draft() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        let draft = schema.get("$schema").and_then(Value::as_str);
        assert!(
            draft.is_some(),
            "{name} missing '$schema' (JSON Schema draft)"
        );
        assert!(
            draft.unwrap().contains("json-schema.org"),
            "{name} has unexpected $schema: {}",
            draft.unwrap()
        );
    }
}

#[test]
fn schema_files_have_id() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        let id = schema.get("$id").and_then(Value::as_str);
        assert!(id.is_some(), "{name} missing '$id'");
        assert!(
            id.unwrap().contains("wezterm-automata.dev"),
            "{name} has unexpected $id domain: {}",
            id.unwrap()
        );
    }
}

#[test]
fn schema_files_no_additional_properties_leak() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    // The envelope schema uses conditional validation (if/then/else) instead
    // of additionalProperties: false, so we skip it.
    let skip = ["wa-robot-envelope.json"];

    for (name, schema) in &schemas {
        if skip.contains(&name.as_str()) {
            continue;
        }
        if schema.get("properties").is_some() {
            let ap = schema.get("additionalProperties");
            assert!(
                ap.is_some(),
                "{name} has properties but no 'additionalProperties' field"
            );
            if let Some(ap_val) = ap {
                assert_eq!(
                    ap_val,
                    &Value::Bool(false),
                    "{name}: 'additionalProperties' should be false"
                );
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Registry ↔ disk coverage
// ─────────────────────────────────────────────────────────────────────

#[test]
fn registry_covers_all_disk_schemas() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let registry = SchemaRegistry::canonical();
    // Exclude the envelope schema — it's a meta-schema (response wrapper),
    // not an endpoint data schema, so it's not in the endpoint registry.
    let disk_names: Vec<String> = schemas
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| name != "wa-robot-envelope.json")
        .collect();

    let uncovered = registry.uncovered_schemas(&disk_names);
    assert!(
        uncovered.is_empty(),
        "Schema files on disk not in registry: {uncovered:?}"
    );
}

#[test]
fn registry_schema_files_exist_on_disk() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let disk_names: HashSet<String> = schemas.into_iter().map(|(name, _)| name).collect();
    let registry = SchemaRegistry::canonical();

    // Track which registry files are missing — these are expected gaps where
    // the schema file hasn't been authored yet.
    let mut missing = Vec::new();
    for file in registry.schema_files() {
        if !disk_names.contains(file) {
            missing.push(file.to_string());
        }
    }

    // Allow known gaps (schemas registered but not yet authored).
    // As schemas are authored, they should be removed from this list.
    let known_gaps: HashSet<&str> = ["wa-robot-rules-lint.json", "wa-robot-rules-show.json"]
        .into_iter()
        .collect();

    let unexpected: Vec<&String> = missing
        .iter()
        .filter(|f| !known_gaps.contains(f.as_str()))
        .collect();

    assert!(
        unexpected.is_empty(),
        "Registry references schema files not on disk (and not in known gaps): {unexpected:?}"
    );
}

#[test]
fn every_endpoint_has_schema_file() {
    let registry = SchemaRegistry::canonical();

    for ep in &registry.endpoints {
        assert!(
            !ep.schema_file.is_empty(),
            "Endpoint '{}' has empty schema_file",
            ep.id
        );
        assert!(
            std::path::Path::new(&ep.schema_file)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json")),
            "Endpoint '{}' schema_file should end with .json: {}",
            ep.id,
            ep.schema_file
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Schema parsing validation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn all_schemas_parse_successfully() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        let doc = parse_schema(schema);
        // Every schema should have a non-empty title
        assert!(!doc.title.is_empty(), "{name} parsed with empty title");
    }
}

#[test]
fn parsed_schemas_have_properties() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    for (name, schema) in &schemas {
        // Envelope and data schemas should have properties
        if schema.get("properties").is_some() {
            let doc = parse_schema(schema);
            assert!(
                !doc.properties.is_empty(),
                "{name} has 'properties' in JSON but parsed to empty"
            );
        }
    }
}

#[test]
fn send_schema_has_expected_fields() {
    let schemas = load_all_schemas();
    let send = schemas
        .iter()
        .find(|(name, _)| name == "wa-robot-send.json");

    if let Some((_, schema)) = send {
        let doc = parse_schema(schema);
        let names: Vec<&str> = doc.properties.iter().map(|p| p.name.as_str()).collect();

        assert!(names.contains(&"pane_id"), "send missing pane_id");
        assert!(names.contains(&"sent"), "send missing sent");
        assert!(
            names.contains(&"policy_decision"),
            "send missing policy_decision"
        );

        // Required fields should be marked required
        let pane_id = doc.properties.iter().find(|p| p.name == "pane_id").unwrap();
        assert!(pane_id.required, "pane_id should be required");

        let policy = doc
            .properties
            .iter()
            .find(|p| p.name == "policy_decision")
            .unwrap();
        assert!(
            !policy.enum_values.is_empty(),
            "policy_decision should have enum values"
        );
    }
}

#[test]
fn events_schema_has_defs() {
    let schemas = load_all_schemas();
    let events = schemas
        .iter()
        .find(|(name, _)| name == "wa-robot-events.json");

    if let Some((_, schema)) = events {
        let doc = parse_schema(schema);
        assert!(
            !doc.definitions.is_empty(),
            "events schema should have $defs"
        );

        assert!(
            doc.definitions.iter().any(|(n, _)| n == "event"),
            "events missing 'event' def"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Docs generation determinism
// ─────────────────────────────────────────────────────────────────────

#[test]
fn docs_generation_deterministic_without_schemas() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig::default();

    let pages1 = generate_reference(&registry, &[], &config);
    let pages2 = generate_reference(&registry, &[], &config);

    assert_eq!(pages1.len(), pages2.len());
    for (p1, p2) in pages1.iter().zip(pages2.iter()) {
        assert_eq!(
            p1.content, p2.content,
            "non-deterministic output for {}",
            p1.filename
        );
    }
}

#[test]
fn docs_generation_deterministic_with_schemas() {
    let schemas = load_all_schemas();
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig::default();

    let pages1 = generate_reference(&registry, &schemas, &config);
    let pages2 = generate_reference(&registry, &schemas, &config);

    assert_eq!(pages1.len(), pages2.len());
    for (p1, p2) in pages1.iter().zip(pages2.iter()) {
        assert_eq!(
            p1.content, p2.content,
            "non-deterministic output for {}",
            p1.filename
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Generated reference structure
// ─────────────────────────────────────────────────────────────────────

#[test]
fn reference_has_header_and_toc() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);

    assert!(!pages.is_empty(), "should produce at least one page");
    let content = &pages[0].content;

    assert!(content.contains("# wa API Reference"), "missing title");
    assert!(content.contains("## Table of Contents"), "missing TOC");
    assert!(
        content.contains(&registry.version),
        "missing version in header"
    );
}

#[test]
fn reference_has_all_categories() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    for cat in EndpointCategory::all() {
        assert!(
            content.contains(cat.title()),
            "missing category section: {}",
            cat.title()
        );
    }
}

#[test]
fn reference_has_envelope_section() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig {
        include_envelope: true,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    assert!(
        content.contains("## Response Envelope"),
        "missing envelope section"
    );
}

#[test]
fn reference_without_envelope() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig {
        include_envelope: false,
        include_error_codes: false,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &[], &config);
    let content = &pages[0].content;

    assert!(
        !content.contains("## Response Envelope"),
        "envelope should be excluded"
    );
}

#[test]
fn reference_has_error_codes() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig {
        include_error_codes: true,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    assert!(
        content.contains("## Error Codes"),
        "missing error codes section"
    );
    assert!(
        content.contains("robot.policy_denied"),
        "missing specific error code"
    );
}

#[test]
fn reference_has_endpoint_sections() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    // Verify a few key endpoints are present
    assert!(content.contains("### Pane State"), "missing Pane State");
    assert!(content.contains("### Send Text"), "missing Send Text");
    assert!(content.contains("### Search"), "missing Search");
    assert!(content.contains("### Run Workflow"), "missing Run Workflow");
    assert!(content.contains("### List Rules"), "missing List Rules");
}

#[test]
fn reference_has_surface_info() {
    let registry = SchemaRegistry::canonical();
    let schemas = load_all_schemas();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    // Dual-surface endpoints should show both robot and MCP
    assert!(
        content.contains("**Robot:** `wa robot state`"),
        "missing robot command for state"
    );
    assert!(
        content.contains("**MCP:** `wa.state`"),
        "missing MCP tool for state"
    );
}

#[test]
fn reference_marks_experimental() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig {
        include_experimental: true,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &[], &config);
    let content = &pages[0].content;

    // rules_show is experimental
    assert!(
        content.contains("Experimental"),
        "should mark experimental endpoints"
    );
}

#[test]
fn reference_excludes_experimental_when_configured() {
    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig {
        include_experimental: false,
        ..Default::default()
    };
    let pages = generate_reference(&registry, &[], &config);
    let content = &pages[0].content;

    // Show Rule is the only experimental endpoint
    assert!(
        !content.contains("### Show Rule"),
        "should exclude experimental endpoints"
    );
}

#[test]
fn reference_has_property_tables_for_loaded_schemas() {
    let schemas = load_all_schemas();
    if schemas.is_empty() {
        return;
    }

    let registry = SchemaRegistry::canonical();
    let config = DocGenConfig::default();
    let pages = generate_reference(&registry, &schemas, &config);
    let content = &pages[0].content;

    // Should have property tables with headers
    assert!(
        content.contains("| Field | Type | Required | Description |"),
        "missing property table headers"
    );

    // Send endpoint should have its fields documented
    assert!(
        content.contains("| `pane_id`"),
        "missing pane_id in property table"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Endpoint categorization coverage
// ─────────────────────────────────────────────────────────────────────

#[test]
fn all_registry_endpoints_categorized() {
    let registry = SchemaRegistry::canonical();

    for ep in &registry.endpoints {
        let cat = categorize_endpoint(ep);
        // Verify it's a valid category (not just Meta for everything)
        let _ = cat.title(); // Should not panic
    }
}

#[test]
fn categorization_covers_expected_distribution() {
    let registry = SchemaRegistry::canonical();

    let mut counts: std::collections::HashMap<EndpointCategory, usize> =
        std::collections::HashMap::new();
    for ep in &registry.endpoints {
        *counts.entry(categorize_endpoint(ep)).or_default() += 1;
    }

    // Sanity check: should have endpoints in multiple categories
    assert!(counts.len() >= 5, "too few categories used: {counts:?}");

    // Pane operations should have 4 (state, get_text, send, wait_for)
    assert_eq!(
        counts
            .get(&EndpointCategory::PaneOperations)
            .copied()
            .unwrap_or(0),
        4,
        "expected 4 pane operations"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Summary generation
// ─────────────────────────────────────────────────────────────────────

#[test]
fn summary_table_includes_all_endpoints() {
    let registry = SchemaRegistry::canonical();
    let summary = generate_endpoint_summary(&registry);

    for ep in &registry.endpoints {
        assert!(
            summary.contains(&ep.title),
            "summary missing endpoint: {}",
            ep.title
        );
    }
}

#[test]
fn summary_table_has_correct_columns() {
    let registry = SchemaRegistry::canonical();
    let summary = generate_endpoint_summary(&registry);

    assert!(summary.contains("| Endpoint |"));
    assert!(summary.contains("| Robot Command |"));
    assert!(summary.contains("| MCP Tool |"));
    assert!(summary.contains("| Stable |"));
}
