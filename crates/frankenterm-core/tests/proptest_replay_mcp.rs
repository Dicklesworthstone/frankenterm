//! Property-based tests for replay_mcp (ft-og6q6.6.3).
//!
//! Invariants tested:
//! - MC-1: ReplayToolSchema serde roundtrip
//! - MC-2: All tool names start with "wa.replay."
//! - MC-3: All schemas have type=object
//! - MC-4: All schemas have additionalProperties=false
//! - MC-5: All schemas have at least one tag
//! - MC-6: All schemas tagged with "replay"
//! - MC-7: schema_for roundtrips all_tool_schemas
//! - MC-8: DispatchResult::Ok serde roundtrip
//! - MC-9: DispatchResult::Error serde roundtrip
//! - MC-10: DispatchResult::Ok is_ok=true, is_error=false
//! - MC-11: DispatchResult::Error is_ok=false, is_error=true
//! - MC-12: validate_required_str rejects empty
//! - MC-13: validate_required_str accepts non-empty
//! - MC-14: validate_optional_u64 uses default when missing
//! - MC-15: All required fields present in schema

use proptest::prelude::*;

use frankenterm_core::replay_mcp::{
    ALL_REPLAY_TOOLS, DispatchResult, ReplayToolSchema, all_tool_schemas, schema_for,
    validate_optional_u64, validate_required_str,
};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── MC-1: Schema serde roundtrip ─────────────────────────────────────

    #[test]
    fn mc1_schema_serde(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            let json = serde_json::to_string(&schema).unwrap();
            let restored: ReplayToolSchema = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(restored, schema);
        }
    }

    // ── MC-2: All names start with wa.replay. ────────────────────────────

    #[test]
    fn mc2_names_namespace(_dummy in 0u8..1) {
        for name in ALL_REPLAY_TOOLS {
            prop_assert!(name.starts_with("wa.replay."), "bad name: {}", name);
        }
    }

    // ── MC-3: All schemas type=object ────────────────────────────────────

    #[test]
    fn mc3_schemas_type_object(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            let ty = schema.input_schema["type"].as_str().unwrap_or("");
            prop_assert_eq!(ty, "object", "schema {} should be object", schema.name);
        }
    }

    // ── MC-4: additionalProperties=false ─────────────────────────────────

    #[test]
    fn mc4_no_additional_properties(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            let addl = schema.input_schema["additionalProperties"].as_bool();
            prop_assert_eq!(addl, Some(false), "schema {} missing additionalProperties", schema.name);
        }
    }

    // ── MC-5: At least one tag ───────────────────────────────────────────

    #[test]
    fn mc5_has_tags(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            prop_assert!(!schema.tags.is_empty(), "schema {} has no tags", schema.name);
        }
    }

    // ── MC-6: Tagged with "replay" ───────────────────────────────────────

    #[test]
    fn mc6_tagged_replay(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            let has_replay = schema.tags.contains(&"replay".to_string());
            prop_assert!(has_replay, "schema {} missing 'replay' tag", schema.name);
        }
    }

    // ── MC-7: schema_for roundtrips ──────────────────────────────────────

    #[test]
    fn mc7_schema_for_roundtrip(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            let found = schema_for(&schema.name);
            prop_assert!(found.is_some(), "schema_for({}) returned None", schema.name);
            prop_assert_eq!(found.unwrap(), schema);
        }
    }

    // ── MC-8: DispatchResult::Ok serde ───────────────────────────────────

    #[test]
    fn mc8_dispatch_ok_serde(val in 0u64..1000) {
        let result = DispatchResult::ok(serde_json::json!({"count": val}));
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── MC-9: DispatchResult::Error serde ────────────────────────────────

    #[test]
    fn mc9_dispatch_error_serde(msg in "[a-z ]{5,20}") {
        let result = DispatchResult::error("replay.test", &msg);
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── MC-10: Ok predicates ─────────────────────────────────────────────

    #[test]
    fn mc10_ok_predicates(val in 0u64..100) {
        let result = DispatchResult::ok(serde_json::json!(val));
        prop_assert!(result.is_ok());
        let is_error = result.is_error();
        prop_assert!(!is_error);
    }

    // ── MC-11: Error predicates ──────────────────────────────────────────

    #[test]
    fn mc11_error_predicates(msg in "[a-z]{3,10}") {
        let result = DispatchResult::error("replay.err", &msg);
        let is_ok = result.is_ok();
        prop_assert!(!is_ok);
        prop_assert!(result.is_error());
    }

    // ── MC-12: validate_required_str rejects empty ───────────────────────

    #[test]
    fn mc12_required_rejects_empty(field in "[a-z]{3,8}") {
        let args = serde_json::json!({&field: ""});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_err());
    }

    // ── MC-13: validate_required_str accepts non-empty ───────────────────

    #[test]
    fn mc13_required_accepts_nonempty(field in "[a-z]{3,8}", val in "[a-z]{3,20}") {
        let args = serde_json::json!({&field: &val});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_ok());
        prop_assert_eq!(result.unwrap(), val);
    }

    // ── MC-14: validate_optional_u64 uses default ────────────────────────

    #[test]
    fn mc14_optional_u64_default(default_val in 0u64..1000) {
        let args = serde_json::json!({});
        let result = validate_optional_u64(&args, "missing", default_val);
        prop_assert_eq!(result, default_val);
    }

    // ── MC-15: Required fields in schema ─────────────────────────────────

    #[test]
    fn mc15_required_fields_in_properties(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            if let Some(required) = schema.input_schema["required"].as_array() {
                let props = schema.input_schema["properties"].as_object().unwrap();
                for req_field in required {
                    let field_name = req_field.as_str().unwrap();
                    prop_assert!(
                        props.contains_key(field_name),
                        "schema {} requires field '{}' not in properties",
                        schema.name, field_name
                    );
                }
            }
        }
    }
}
