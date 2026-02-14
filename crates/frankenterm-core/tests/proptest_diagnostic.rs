//! Property-based tests for the diagnostic module.
//!
//! Tests structural invariants of DiagnosticOptions (Default, Clone) and
//! DiagnosticResult (Serialize validity, Clone equivalence).

use frankenterm_core::diagnostic::{DiagnosticOptions, DiagnosticResult};
use proptest::prelude::*;
use std::path::PathBuf;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_diagnostic_options() -> impl Strategy<Value = DiagnosticOptions> {
    (
        1usize..1000,                                             // event_limit
        1usize..500,                                              // audit_limit
        1usize..500,                                              // workflow_limit
        prop::option::of("[a-z/]{1,30}".prop_map(PathBuf::from)), // output
    )
        .prop_map(
            |(event_limit, audit_limit, workflow_limit, output)| DiagnosticOptions {
                event_limit,
                audit_limit,
                workflow_limit,
                output,
            },
        )
}

fn arb_diagnostic_result() -> impl Strategy<Value = DiagnosticResult> {
    (
        "[a-z/_.-]{1,50}", // output_path
        0usize..100,       // file_count
        0u64..10_000_000,  // total_size_bytes
    )
        .prop_map(
            |(output_path, file_count, total_size_bytes)| DiagnosticResult {
                output_path,
                file_count,
                total_size_bytes,
            },
        )
}

// ── DiagnosticOptions: Default ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default options have specific known values.
    #[test]
    fn options_default_values(_i in 0..1u8) {
        let d = DiagnosticOptions::default();
        prop_assert_eq!(d.event_limit, 100, "default event_limit should be 100");
        prop_assert_eq!(d.audit_limit, 50, "default audit_limit should be 50");
        prop_assert_eq!(d.workflow_limit, 50, "default workflow_limit should be 50");
        prop_assert!(d.output.is_none(), "default output should be None");
    }

    /// Default is deterministic.
    #[test]
    fn options_default_deterministic(_i in 0..1u8) {
        let a = DiagnosticOptions::default();
        let b = DiagnosticOptions::default();
        prop_assert_eq!(a.event_limit, b.event_limit);
        prop_assert_eq!(a.audit_limit, b.audit_limit);
        prop_assert_eq!(a.workflow_limit, b.workflow_limit);
        prop_assert_eq!(a.output, b.output);
    }
}

// ── DiagnosticOptions: Clone ────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces equivalent options.
    #[test]
    fn options_clone(opts in arb_diagnostic_options()) {
        let cloned = opts.clone();
        prop_assert_eq!(cloned.event_limit, opts.event_limit);
        prop_assert_eq!(cloned.audit_limit, opts.audit_limit);
        prop_assert_eq!(cloned.workflow_limit, opts.workflow_limit);
        prop_assert_eq!(cloned.output, opts.output);
    }

    /// Debug format is non-empty.
    #[test]
    fn options_debug_non_empty(opts in arb_diagnostic_options()) {
        let debug = format!("{:?}", opts);
        prop_assert!(!debug.is_empty());
    }

    /// Debug format contains key field names.
    #[test]
    fn options_debug_contains_fields(opts in arb_diagnostic_options()) {
        let debug = format!("{:?}", opts);
        prop_assert!(debug.contains("event_limit"), "Debug should mention 'event_limit'");
        prop_assert!(debug.contains("audit_limit"), "Debug should mention 'audit_limit'");
        prop_assert!(debug.contains("workflow_limit"), "Debug should mention 'workflow_limit'");
    }

    /// All limits are positive in generated options.
    #[test]
    fn options_limits_positive(opts in arb_diagnostic_options()) {
        prop_assert!(opts.event_limit > 0, "event_limit should be positive");
        prop_assert!(opts.audit_limit > 0, "audit_limit should be positive");
        prop_assert!(opts.workflow_limit > 0, "workflow_limit should be positive");
    }
}

// ── DiagnosticResult: Serialize ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Serialized result is valid JSON.
    #[test]
    fn result_serialize_valid_json(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Required fields are present in serialized JSON.
    #[test]
    fn result_serialize_has_required_fields(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("output_path").is_some(), "missing 'output_path'");
        prop_assert!(value.get("file_count").is_some(), "missing 'file_count'");
        prop_assert!(value.get("total_size_bytes").is_some(), "missing 'total_size_bytes'");
    }

    /// Output path is preserved in JSON.
    #[test]
    fn result_serialize_preserves_path(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let path = value.get("output_path").unwrap().as_str().unwrap();
        prop_assert_eq!(path, r.output_path.as_str());
    }

    /// Numeric fields are preserved in JSON.
    #[test]
    fn result_serialize_preserves_numbers(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let file_count = value.get("file_count").unwrap().as_u64().unwrap() as usize;
        let size = value.get("total_size_bytes").unwrap().as_u64().unwrap();
        prop_assert_eq!(file_count, r.file_count);
        prop_assert_eq!(size, r.total_size_bytes);
    }

    /// Pretty-printed JSON is also valid.
    #[test]
    fn result_serialize_pretty(r in arb_diagnostic_result()) {
        let json = serde_json::to_string_pretty(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Serialization is deterministic.
    #[test]
    fn result_serialize_deterministic(r in arb_diagnostic_result()) {
        let j1 = serde_json::to_string(&r).unwrap();
        let j2 = serde_json::to_string(&r).unwrap();
        prop_assert_eq!(j1.as_str(), j2.as_str());
    }

    /// Output path field is a string in JSON.
    #[test]
    fn result_serialize_path_is_string(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("output_path").unwrap().is_string(),
            "output_path should be a string in JSON");
    }
}

// ── DiagnosticResult: Clone / Debug ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces equivalent result.
    #[test]
    fn result_clone(r in arb_diagnostic_result()) {
        let cloned = r.clone();
        prop_assert_eq!(cloned.output_path.as_str(), r.output_path.as_str());
        prop_assert_eq!(cloned.file_count, r.file_count);
        prop_assert_eq!(cloned.total_size_bytes, r.total_size_bytes);
    }

    /// Debug format is non-empty.
    #[test]
    fn result_debug_non_empty(r in arb_diagnostic_result()) {
        let debug = format!("{:?}", r);
        prop_assert!(!debug.is_empty());
    }
}

// ── DiagnosticOptions: structural invariants ────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone is field-identical to original.
    #[test]
    fn options_clone_field_identical(opts in arb_diagnostic_options()) {
        let c = opts.clone();
        prop_assert_eq!(c.event_limit, opts.event_limit);
        prop_assert_eq!(c.audit_limit, opts.audit_limit);
        prop_assert_eq!(c.workflow_limit, opts.workflow_limit);
        prop_assert_eq!(c.output, opts.output);
    }

    /// Debug contains type name.
    #[test]
    fn options_debug_contains_type(opts in arb_diagnostic_options()) {
        let dbg = format!("{:?}", opts);
        prop_assert!(dbg.contains("DiagnosticOptions"));
    }

    /// Default clone is field-identical.
    #[test]
    fn options_default_clone(_dummy in 0..1_u8) {
        let d = DiagnosticOptions::default();
        let c = d.clone();
        prop_assert_eq!(c.event_limit, d.event_limit);
        prop_assert_eq!(c.audit_limit, d.audit_limit);
        prop_assert_eq!(c.workflow_limit, d.workflow_limit);
        prop_assert_eq!(c.output, d.output);
    }

    /// Options with output=Some preserve the path through clone.
    #[test]
    fn options_clone_with_output(path in "[a-z/]{3,20}") {
        let opts = DiagnosticOptions {
            event_limit: 50,
            audit_limit: 25,
            workflow_limit: 10,
            output: Some(PathBuf::from(&path)),
        };
        let cloned = opts.clone();
        prop_assert_eq!(
            cloned.output.as_ref().map(|p| p.to_str().unwrap()),
            Some(path.as_str())
        );
    }

    /// Options with output=None clone correctly.
    #[test]
    fn options_clone_without_output(el in 1usize..500) {
        let opts = DiagnosticOptions {
            event_limit: el,
            audit_limit: 1,
            workflow_limit: 1,
            output: None,
        };
        let cloned = opts.clone();
        prop_assert!(cloned.output.is_none());
        prop_assert_eq!(cloned.event_limit, el);
    }

    /// event_limit is always >= audit_limit and workflow_limit in defaults.
    #[test]
    fn options_default_limit_ordering(_dummy in 0..1_u8) {
        let d = DiagnosticOptions::default();
        prop_assert!(d.event_limit >= d.audit_limit);
        prop_assert!(d.event_limit >= d.workflow_limit);
    }
}

// ── DiagnosticResult: additional serialization ──────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone and serialize produce identical JSON.
    #[test]
    fn result_clone_serialize_identical(r in arb_diagnostic_result()) {
        let json1 = serde_json::to_string(&r).unwrap();
        let json2 = serde_json::to_string(&r.clone()).unwrap();
        prop_assert_eq!(json1, json2);
    }

    /// Result JSON field count is exactly 3.
    #[test]
    fn result_json_field_count(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert_eq!(obj.len(), 3, "DiagnosticResult should have 3 fields");
    }

    /// Result Debug contains the type name.
    #[test]
    fn result_debug_contains_type(r in arb_diagnostic_result()) {
        let dbg = format!("{:?}", r);
        prop_assert!(dbg.contains("DiagnosticResult"));
    }

    /// Pretty JSON is longer than compact JSON.
    #[test]
    fn result_pretty_longer(r in arb_diagnostic_result()) {
        let compact = serde_json::to_string(&r).unwrap();
        let pretty = serde_json::to_string_pretty(&r).unwrap();
        prop_assert!(pretty.len() >= compact.len(),
            "pretty {} should be >= compact {}", pretty.len(), compact.len());
    }

    /// Result JSON is valid UTF-8.
    #[test]
    fn result_json_valid_utf8(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        prop_assert!(std::str::from_utf8(json.as_bytes()).is_ok());
    }

    /// file_count is preserved through JSON Value.
    #[test]
    fn result_value_preserves_file_count(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let fc = value.get("file_count").unwrap().as_u64().unwrap() as usize;
        prop_assert_eq!(fc, r.file_count);
    }

    /// total_size_bytes is preserved through JSON Value.
    #[test]
    fn result_value_preserves_size(r in arb_diagnostic_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let sz = value.get("total_size_bytes").unwrap().as_u64().unwrap();
        prop_assert_eq!(sz, r.total_size_bytes);
    }
}
