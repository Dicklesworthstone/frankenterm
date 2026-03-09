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
    validate_optional_str, validate_optional_u64, validate_required_str,
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

// =============================================================================
// Additional coverage tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// MC-16: DispatchResult::error_with_hint serde roundtrip.
    #[test]
    fn mc16_error_with_hint_serde(
        code in "[a-z.]{5,20}",
        msg in "[a-zA-Z0-9 ]{5,30}",
        hint in "[a-zA-Z0-9 ]{5,30}",
    ) {
        let result = DispatchResult::error_with_hint(&code, &msg, &hint);
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored, &result);
        // Verify hint is preserved
        if let DispatchResult::Error { hint: h, .. } = &restored {
            prop_assert_eq!(h.as_deref(), Some(hint.as_str()));
        } else {
            prop_assert!(false, "wrong variant after roundtrip");
        }
    }

    /// MC-17: DispatchResult::error_with_hint predicates.
    #[test]
    fn mc17_error_with_hint_predicates(
        msg in "[a-z]{3,10}",
        hint in "[a-z]{3,10}",
    ) {
        let result = DispatchResult::error_with_hint("replay.err", &msg, &hint);
        let is_ok = result.is_ok();
        prop_assert!(!is_ok);
        prop_assert!(result.is_error());
    }

    /// MC-18: validate_optional_str returns Some for non-empty strings.
    #[test]
    fn mc18_optional_str_present(
        field in "[a-z]{3,8}",
        val in "[a-z]{3,20}",
    ) {
        let args = serde_json::json!({&field: &val});
        let result = validate_optional_str(&args, &field);
        prop_assert_eq!(result, Some(val));
    }

    /// MC-19: validate_optional_str returns None for missing field.
    #[test]
    fn mc19_optional_str_missing(field in "[a-z]{3,8}") {
        let args = serde_json::json!({});
        let result = validate_optional_str(&args, &field);
        prop_assert_eq!(result, None);
    }

    /// MC-20: validate_optional_str returns None for empty/whitespace strings.
    #[test]
    fn mc20_optional_str_whitespace(field in "[a-z]{3,8}") {
        let args = serde_json::json!({&field: "   "});
        let result = validate_optional_str(&args, &field);
        prop_assert_eq!(result, None);

        let args_empty = serde_json::json!({&field: ""});
        let result_empty = validate_optional_str(&args_empty, &field);
        prop_assert_eq!(result_empty, None);
    }

    /// MC-21: validate_optional_u64 uses provided value when present.
    #[test]
    fn mc21_optional_u64_present(
        val in 0u64..100_000,
        default_val in 0u64..100_000,
    ) {
        let args = serde_json::json!({"field": val});
        let result = validate_optional_u64(&args, "field", default_val);
        prop_assert_eq!(result, val);
    }

    /// MC-22: validate_required_str rejects whitespace-only.
    #[test]
    fn mc22_required_rejects_whitespace(
        field in "[a-z]{3,8}",
        spaces in 1..10usize,
    ) {
        let ws: String = " ".repeat(spaces);
        let args = serde_json::json!({&field: ws});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_err());
    }

    /// MC-23: validate_required_str trims and returns trimmed value.
    #[test]
    fn mc23_required_str_trims(
        field in "[a-z]{3,8}",
        val in "[a-z]{3,15}",
    ) {
        let padded = format!("  {val}  ");
        let args = serde_json::json!({&field: padded});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_ok());
        prop_assert_eq!(result.unwrap(), val);
    }

    /// MC-24: schema_for returns None for unknown tool names.
    #[test]
    fn mc24_schema_for_unknown(name in "[a-z]{5,15}") {
        // Prefix with something that won't collide with real tools
        let fake_name = format!("wa.fake.{name}");
        let result = schema_for(&fake_name);
        prop_assert!(result.is_none());
    }

    /// MC-25: Arbitrary ReplayToolSchema serde roundtrip.
    #[test]
    fn mc25_arbitrary_schema_serde(
        name in "[a-z.]{5,20}",
        description in "[a-zA-Z0-9 ]{10,50}",
        tags in prop::collection::vec("[a-z]{3,8}", 1..5),
    ) {
        let schema = ReplayToolSchema {
            name,
            description,
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            tags,
        };
        let json = serde_json::to_string(&schema).unwrap();
        let restored: ReplayToolSchema = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&restored, &schema);
    }

    /// MC-26: All schema descriptions are non-empty.
    #[test]
    fn mc26_descriptions_non_empty(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            prop_assert!(
                !schema.description.is_empty(),
                "schema {} has empty description", schema.name
            );
        }
    }

    /// MC-27: ALL_REPLAY_TOOLS has exactly 6 entries and all are unique.
    #[test]
    fn mc27_tool_names_count_and_unique(_dummy in 0u8..1) {
        prop_assert_eq!(ALL_REPLAY_TOOLS.len(), 6);
        let mut names: Vec<&str> = ALL_REPLAY_TOOLS.to_vec();
        names.sort();
        names.dedup();
        prop_assert_eq!(names.len(), 6);
    }

    /// MC-28: DispatchResult::error without hint has hint=None after serde.
    #[test]
    fn mc28_error_no_hint_serde(
        code in "[a-z.]{5,15}",
        msg in "[a-z ]{5,20}",
    ) {
        let result = DispatchResult::error(&code, &msg);
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        if let DispatchResult::Error { hint, code: c, message: m } = &restored {
            prop_assert!(hint.is_none(), "hint should be None, got {:?}", hint);
            prop_assert_eq!(c, &code);
            prop_assert_eq!(m, &msg);
        } else {
            prop_assert!(false, "wrong variant");
        }
    }

    /// MC-29: validate_required_str rejects non-string types.
    #[test]
    fn mc29_required_str_rejects_non_string(
        field in "[a-z]{3,8}",
        num in 0i64..1000,
    ) {
        let args = serde_json::json!({&field: num});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_err());
    }

    /// MC-30: validate_optional_u64 returns default for non-integer types.
    #[test]
    fn mc30_optional_u64_non_integer(
        default_val in 0u64..100_000,
        val in "[a-z]{3,10}",
    ) {
        let args = serde_json::json!({"field": val});
        let result = validate_optional_u64(&args, "field", default_val);
        prop_assert_eq!(result, default_val);
    }
}
