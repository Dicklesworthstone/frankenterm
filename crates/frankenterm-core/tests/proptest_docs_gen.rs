//! Property-based tests for the `docs_gen` module.
//!
//! Verifies schema parsing invariants (property sorting, definition ordering,
//! type extraction), endpoint categorization determinism, and reference
//! generation consistency.

use serde_json::{Value, json};

use frankenterm_core::api_schema::EndpointMeta;
use frankenterm_core::docs_gen::{
    DocGenConfig, EndpointCategory, categorize_endpoint, parse_schema,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

/// Generate an arbitrary JSON Schema property with varying types.
fn arb_schema_property() -> impl Strategy<Value = Value> {
    prop_oneof![
        // string property
        "[a-z]{0,50}".prop_map(|desc| json!({
            "type": "string",
            "description": desc,
        })),
        // integer property with min/max
        (0i64..1000, 1000i64..10_000).prop_map(|(min, max)| json!({
            "type": "integer",
            "description": "A numeric field",
            "minimum": min,
            "maximum": max,
        })),
        // boolean property
        Just(json!({
            "type": "boolean",
            "description": "A flag",
        })),
        // array of strings
        Just(json!({
            "type": "array",
            "items": { "type": "string" },
            "description": "A list of strings",
        })),
        // enum property
        proptest::collection::vec("[a-z_]{1,10}", 2..5).prop_map(|variants| {
            json!({
                "type": "string",
                "enum": variants,
                "description": "Enumerated values",
            })
        }),
        // $ref property
        "[A-Z][a-z]{2,10}".prop_map(|name| json!({
            "$ref": format!("#/$defs/{name}"),
        })),
    ]
}

/// Generate a JSON Schema object with random properties and optional $defs.
fn arb_json_schema() -> impl Strategy<Value = Value> {
    let prop_names = proptest::collection::vec("[a-z_]{1,12}", 0..8);
    let title = "[A-Z][a-zA-Z]{0,20}";
    let desc = "[a-z ]{0,50}";

    (prop_names, title, desc)
        .prop_flat_map(|(names, title, desc)| {
            let n = names.len();
            let props = proptest::collection::vec(arb_schema_property(), n..=n);
            // Randomly select some properties as required
            let required_mask = proptest::collection::vec(any::<bool>(), n..=n);

            (Just(names), props, required_mask, Just(title), Just(desc))
        })
        .prop_map(|(names, props, required_mask, title, desc)| {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for (i, name) in names.iter().enumerate() {
                properties.insert(name.clone(), props[i].clone());
                if required_mask[i] {
                    required.push(name.clone());
                }
            }

            json!({
                "title": title,
                "description": desc,
                "properties": properties,
                "required": required,
            })
        })
}

/// Generate an EndpointMeta with a random or known endpoint ID.
fn arb_endpoint_meta() -> impl Strategy<Value = EndpointMeta> {
    let known_ids = prop_oneof![
        Just("state".to_string()),
        Just("get_text".to_string()),
        Just("send".to_string()),
        Just("wait_for".to_string()),
        Just("search".to_string()),
        Just("events".to_string()),
        Just("workflow_run".to_string()),
        Just("workflow_list".to_string()),
        Just("rules_list".to_string()),
        Just("rules_test".to_string()),
        Just("accounts_list".to_string()),
        Just("reserve".to_string()),
        Just("release".to_string()),
        Just("help".to_string()),
        Just("approve".to_string()),
        "[a-z_]{1,20}".prop_map(|s| s),
    ];

    (
        known_ids,
        "[A-Z][a-zA-Z ]{0,30}",
        "[a-z ]{0,50}",
        any::<bool>(),
    )
        .prop_map(|(id, title, desc, stable)| EndpointMeta {
            id,
            title,
            description: desc,
            robot_command: Some("robot cmd".to_string()),
            mcp_tool: Some("wa.tool".to_string()),
            schema_file: "wa-robot-test.json".to_string(),
            stable,
            since: "0.1.0".to_string(),
        })
}

fn arb_endpoint_category() -> impl Strategy<Value = EndpointCategory> {
    prop_oneof![
        Just(EndpointCategory::PaneOperations),
        Just(EndpointCategory::SearchAndEvents),
        Just(EndpointCategory::Workflows),
        Just(EndpointCategory::Rules),
        Just(EndpointCategory::Accounts),
        Just(EndpointCategory::Reservations),
        Just(EndpointCategory::Meta),
    ]
}

// =========================================================================
// parse_schema properties
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Properties are sorted: required first (alphabetically), then optional (alphabetically).
    #[test]
    fn prop_parse_schema_properties_sorted(schema in arb_json_schema()) {
        let doc = parse_schema(&schema);
        for window in doc.properties.windows(2) {
            let a = &window[0];
            let b = &window[1];
            // Required comes before non-required
            if a.required && !b.required {
                // OK: required before optional
            } else if !a.required && b.required {
                prop_assert!(false,
                    "non-required '{}' appeared before required '{}'",
                    a.name, b.name);
            } else {
                // Same required-ness: alphabetical
                prop_assert!(a.name <= b.name,
                    "'{}' should come before '{}'", a.name, b.name);
            }
        }
    }

    /// Every property in the schema appears in the parsed output.
    #[test]
    fn prop_parse_schema_no_property_lost(schema in arb_json_schema()) {
        let doc = parse_schema(&schema);
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            prop_assert_eq!(doc.properties.len(), props.len(),
                "parsed {} properties but schema has {}",
                doc.properties.len(), props.len());
            for (name, _) in props {
                prop_assert!(
                    doc.properties.iter().any(|p| p.name == *name),
                    "property '{}' missing from parsed output", name
                );
            }
        }
    }

    /// Required fields are correctly identified.
    #[test]
    fn prop_parse_schema_required_fields(schema in arb_json_schema()) {
        let doc = parse_schema(&schema);
        let required_set: Vec<String> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default();

        for prop in &doc.properties {
            prop_assert_eq!(
                prop.required,
                required_set.contains(&prop.name),
                "property '{}' required mismatch", prop.name
            );
        }
    }

    /// Title and description are extracted correctly.
    #[test]
    fn prop_parse_schema_title_description(schema in arb_json_schema()) {
        let doc = parse_schema(&schema);
        let expected_title = schema
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("");
        let expected_desc = schema
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        prop_assert_eq!(doc.title.as_str(), expected_title);
        prop_assert_eq!(doc.description.as_str(), expected_desc);
    }
}

// =========================================================================
// parse_schema with $defs
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// $defs are sorted alphabetically by name.
    #[test]
    fn prop_parse_schema_defs_sorted(
        def_names in proptest::collection::vec("[A-Z][a-z]{2,10}", 1..5),
    ) {
        let mut defs = serde_json::Map::new();
        for name in &def_names {
            defs.insert(name.clone(), json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            }));
        }
        let schema = json!({
            "title": "Test",
            "$defs": defs,
        });
        let doc = parse_schema(&schema);
        for window in doc.definitions.windows(2) {
            prop_assert!(window[0].0 <= window[1].0,
                "def '{}' should come before '{}'",
                window[0].0, window[1].0);
        }
    }

    /// Number of $defs matches input.
    #[test]
    fn prop_parse_schema_defs_count(
        def_names in proptest::collection::hash_set("[A-Z][a-z]{2,10}", 0..6),
    ) {
        let mut defs = serde_json::Map::new();
        for name in &def_names {
            defs.insert(name.clone(), json!({ "type": "object" }));
        }
        let schema = json!({ "$defs": defs });
        let doc = parse_schema(&schema);
        prop_assert_eq!(doc.definitions.len(), def_names.len());
    }
}

// =========================================================================
// parse_schema edge cases
// =========================================================================

#[test]
fn parse_schema_empty_object() {
    let doc = parse_schema(&json!({}));
    assert!(doc.title.is_empty());
    assert!(doc.description.is_empty());
    assert!(doc.properties.is_empty());
    assert!(doc.definitions.is_empty());
}

#[test]
fn parse_schema_null_value() {
    let doc = parse_schema(&Value::Null);
    assert!(doc.title.is_empty());
    assert!(doc.properties.is_empty());
}

#[test]
fn parse_schema_array_type_property() {
    let schema = json!({
        "properties": {
            "tags": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Tags"
            }
        }
    });
    let doc = parse_schema(&schema);
    assert_eq!(doc.properties.len(), 1);
    assert_eq!(doc.properties[0].type_str, "string[]");
}

#[test]
fn parse_schema_ref_property() {
    let schema = json!({
        "properties": {
            "config": {
                "$ref": "#/$defs/MyConfig"
            }
        }
    });
    let doc = parse_schema(&schema);
    assert_eq!(doc.properties.len(), 1);
    assert_eq!(doc.properties[0].type_str, "MyConfig");
}

#[test]
fn parse_schema_nullable_type() {
    let schema = json!({
        "properties": {
            "value": {
                "type": ["integer", "null"],
                "description": "Optional int"
            }
        }
    });
    let doc = parse_schema(&schema);
    assert_eq!(doc.properties[0].type_str, "integer | null");
}

#[test]
fn parse_schema_enum_property() {
    let schema = json!({
        "properties": {
            "status": {
                "type": "string",
                "enum": ["active", "inactive", "pending"],
                "description": "Current status"
            }
        }
    });
    let doc = parse_schema(&schema);
    assert_eq!(
        doc.properties[0].enum_values,
        vec!["active", "inactive", "pending"]
    );
}

#[test]
fn parse_schema_min_max_constraints() {
    let schema = json!({
        "properties": {
            "count": {
                "type": "integer",
                "minimum": 0,
                "maximum": 100,
                "description": "A count"
            }
        }
    });
    let doc = parse_schema(&schema);
    assert_eq!(doc.properties[0].minimum, Some(0.0));
    assert_eq!(doc.properties[0].maximum, Some(100.0));
}

// =========================================================================
// categorize_endpoint
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Categorization is deterministic.
    #[test]
    fn prop_categorize_deterministic(ep in arb_endpoint_meta()) {
        let cat1 = categorize_endpoint(&ep);
        let cat2 = categorize_endpoint(&ep);
        prop_assert_eq!(cat1, cat2);
    }

    /// Unknown endpoint IDs always map to Meta.
    #[test]
    fn prop_unknown_endpoint_is_meta(
        id in "[a-z]{10,20}_unknown",
    ) {
        let ep = EndpointMeta {
            id,
            title: "Test".to_string(),
            description: "Test".to_string(),
            robot_command: None,
            mcp_tool: None,
            schema_file: "test.json".to_string(),
            stable: true,
            since: "0.1.0".to_string(),
        };
        prop_assert_eq!(categorize_endpoint(&ep), EndpointCategory::Meta);
    }
}

// =========================================================================
// categorize_endpoint: known mappings
// =========================================================================

#[test]
fn categorize_pane_operations() {
    for id in ["state", "get_text", "send", "wait_for"] {
        let ep = EndpointMeta {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: String::new(),
        };
        assert_eq!(
            categorize_endpoint(&ep),
            EndpointCategory::PaneOperations,
            "endpoint '{}' should be PaneOperations",
            id
        );
    }
}

#[test]
fn categorize_search_and_events() {
    for id in [
        "search",
        "events",
        "events_annotate",
        "events_triage",
        "events_label",
    ] {
        let ep = EndpointMeta {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: String::new(),
        };
        assert_eq!(
            categorize_endpoint(&ep),
            EndpointCategory::SearchAndEvents,
            "endpoint '{}' should be SearchAndEvents",
            id
        );
    }
}

#[test]
fn categorize_workflows() {
    for id in [
        "workflow_run",
        "workflow_list",
        "workflow_status",
        "workflow_abort",
    ] {
        let ep = EndpointMeta {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: String::new(),
        };
        assert_eq!(
            categorize_endpoint(&ep),
            EndpointCategory::Workflows,
            "endpoint '{}' should be Workflows",
            id
        );
    }
}

#[test]
fn categorize_rules() {
    for id in ["rules_list", "rules_test", "rules_show", "rules_lint"] {
        let ep = EndpointMeta {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: String::new(),
        };
        assert_eq!(
            categorize_endpoint(&ep),
            EndpointCategory::Rules,
            "endpoint '{}' should be Rules",
            id
        );
    }
}

#[test]
fn categorize_accounts() {
    for id in ["accounts_list", "accounts_refresh"] {
        let ep = EndpointMeta {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: String::new(),
        };
        assert_eq!(
            categorize_endpoint(&ep),
            EndpointCategory::Accounts,
            "endpoint '{}' should be Accounts",
            id
        );
    }
}

#[test]
fn categorize_reservations() {
    for id in ["reservations_list", "reserve", "release"] {
        let ep = EndpointMeta {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            robot_command: None,
            mcp_tool: None,
            schema_file: String::new(),
            stable: true,
            since: String::new(),
        };
        assert_eq!(
            categorize_endpoint(&ep),
            EndpointCategory::Reservations,
            "endpoint '{}' should be Reservations",
            id
        );
    }
}

// =========================================================================
// EndpointCategory
// =========================================================================

#[test]
fn endpoint_category_all_returns_seven() {
    assert_eq!(EndpointCategory::all().len(), 7);
}

#[test]
fn endpoint_category_titles_nonempty_and_distinct() {
    let titles: Vec<&str> = EndpointCategory::all().iter().map(|c| c.title()).collect();
    for title in &titles {
        assert!(!title.is_empty());
    }
    // All distinct
    let mut sorted = titles.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), titles.len());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prop_category_serde_roundtrip(cat in arb_endpoint_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let parsed: EndpointCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, parsed);
    }
}

// =========================================================================
// DocGenConfig
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn prop_doc_gen_config_serde_roundtrip(
        envelope in any::<bool>(),
        experimental in any::<bool>(),
        error_codes in any::<bool>(),
    ) {
        let config = DocGenConfig {
            include_envelope: envelope,
            include_experimental: experimental,
            include_error_codes: error_codes,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: DocGenConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.include_envelope, parsed.include_envelope);
        prop_assert_eq!(config.include_experimental, parsed.include_experimental);
        prop_assert_eq!(config.include_error_codes, parsed.include_error_codes);
    }
}
