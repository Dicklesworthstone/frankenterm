//! Property-based tests for export module
//!
//! Tests: ExportKind (from_str_loose/as_str/all_names roundtrips, case insensitivity,
//! alias acceptance, rejection of unknown), ExportHeader (serde field names, optional
//! field skipping, required field presence, value preservation).

use frankenterm_core::export::{ExportHeader, ExportKind, ExportOptions};
use frankenterm_core::storage::ExportQuery;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

/// Generate arbitrary ExportKind variant.
fn arb_export_kind() -> impl Strategy<Value = ExportKind> {
    prop_oneof![
        Just(ExportKind::Segments),
        Just(ExportKind::Gaps),
        Just(ExportKind::Events),
        Just(ExportKind::Workflows),
        Just(ExportKind::Sessions),
        Just(ExportKind::Audit),
        Just(ExportKind::Reservations),
    ]
}

/// Generate known aliases that should parse to ExportKind variants.
fn arb_alias() -> impl Strategy<Value = (&'static str, ExportKind)> {
    prop_oneof![
        Just(("segments", ExportKind::Segments)),
        Just(("segment", ExportKind::Segments)),
        Just(("output", ExportKind::Segments)),
        Just(("gaps", ExportKind::Gaps)),
        Just(("gap", ExportKind::Gaps)),
        Just(("events", ExportKind::Events)),
        Just(("event", ExportKind::Events)),
        Just(("detections", ExportKind::Events)),
        Just(("workflows", ExportKind::Workflows)),
        Just(("workflow", ExportKind::Workflows)),
        Just(("sessions", ExportKind::Sessions)),
        Just(("session", ExportKind::Sessions)),
        Just(("audit", ExportKind::Audit)),
        Just(("audit_actions", ExportKind::Audit)),
        Just(("audit-actions", ExportKind::Audit)),
        Just(("reservations", ExportKind::Reservations)),
        Just(("reservation", ExportKind::Reservations)),
        Just(("reserves", ExportKind::Reservations)),
    ]
}

/// Generate arbitrary ExportHeader.
fn arb_export_header() -> impl Strategy<Value = ExportHeader> {
    (
        "0\\.[0-9]+\\.[0-9]+",
        arb_export_kind(),
        proptest::bool::ANY,
        1_000_000_000_000i64..2_000_000_000_000i64,
        proptest::option::of(0..1000u64),
        proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        proptest::option::of(1..10000usize),
        0..10000usize,
    )
        .prop_map(
            |(
                version,
                kind,
                redacted,
                exported_at_ms,
                pane_id,
                since,
                until,
                limit,
                record_count,
            )| {
                ExportHeader {
                    export: true,
                    version,
                    kind: kind.as_str().to_string(),
                    redacted,
                    exported_at_ms,
                    pane_id,
                    since,
                    until,
                    limit,
                    record_count,
                }
            },
        )
}

/// Generate random strings that should NOT parse as ExportKind.
fn arb_invalid_kind_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just("unknown".to_string()),
        Just("foo".to_string()),
        Just("INVALID".to_string()),
        Just("seg".to_string()),
        Just("ev".to_string()),
        Just("wf".to_string()),
        Just("audits".to_string()),
        Just("reserve".to_string()),
        "x{5,20}",
    ]
}

// ============================================================================
// Property Tests: ExportKind
// ============================================================================

proptest! {
    /// Property 1: as_str/from_str_loose roundtrip — every variant survives roundtrip
    #[test]
    fn prop_export_kind_roundtrip(kind in arb_export_kind()) {
        let s = kind.as_str();
        let back = ExportKind::from_str_loose(s);
        prop_assert_eq!(back, Some(kind),
            "Roundtrip failed for {:?} -> {} -> {:?}", kind, s, back);
    }

    /// Property 2: as_str always returns non-empty lowercase string
    #[test]
    fn prop_export_kind_as_str_nonempty_lowercase(kind in arb_export_kind()) {
        let s = kind.as_str();
        prop_assert!(!s.is_empty(), "as_str should return non-empty string");
        let lower = s.to_lowercase();
        prop_assert_eq!(s, lower.as_str(),
            "as_str should return lowercase: got {}", s);
    }

    /// Property 3: from_str_loose is case-insensitive
    #[test]
    fn prop_export_kind_case_insensitive(kind in arb_export_kind()) {
        let s = kind.as_str();
        let upper = s.to_uppercase();
        let mixed = s.chars().enumerate()
            .map(|(i, c)| if i % 2 == 0 { c.to_uppercase().next().unwrap() } else { c })
            .collect::<String>();

        prop_assert_eq!(ExportKind::from_str_loose(&upper), Some(kind),
            "UPPER case '{}' should parse to {:?}", upper, kind);
        prop_assert_eq!(ExportKind::from_str_loose(&mixed), Some(kind),
            "Mixed case '{}' should parse to {:?}", mixed, kind);
    }

    /// Property 4: All known aliases parse correctly
    #[test]
    fn prop_export_kind_aliases_parse((alias, expected) in arb_alias()) {
        let result = ExportKind::from_str_loose(alias);
        prop_assert_eq!(result, Some(expected),
            "Alias '{}' should parse to {:?}, got {:?}", alias, expected, result);
    }

    /// Property 5: Aliases are case-insensitive too
    #[test]
    fn prop_export_kind_aliases_case_insensitive((alias, expected) in arb_alias()) {
        let upper = alias.to_uppercase();
        let result = ExportKind::from_str_loose(&upper);
        prop_assert_eq!(result, Some(expected),
            "Uppercase alias '{}' should parse to {:?}", upper, expected);
    }

    /// Property 6: Invalid strings are rejected
    #[test]
    fn prop_export_kind_rejects_invalid(s in arb_invalid_kind_string()) {
        let result = ExportKind::from_str_loose(&s);
        prop_assert_eq!(result, None,
            "Invalid string '{}' should return None, got {:?}", s, result);
    }

    /// Property 7: All variant as_str values are distinct
    #[test]
    fn prop_export_kind_as_str_distinct(k1 in arb_export_kind(), k2 in arb_export_kind()) {
        if k1 != k2 {
            prop_assert_ne!(k1.as_str(), k2.as_str(),
                "{:?} and {:?} should have different as_str", k1, k2);
        }
    }

    /// Property 8: all_names contains every variant's as_str
    #[test]
    fn prop_export_kind_all_names_contains_variant(kind in arb_export_kind()) {
        let names = ExportKind::all_names();
        prop_assert!(names.contains(&kind.as_str()),
            "all_names should contain '{}' for {:?}", kind.as_str(), kind);
    }

    /// Property 9: Every entry in all_names round-trips via from_str_loose
    #[test]
    fn prop_export_kind_all_names_parse(_dummy in Just(())) {
        for name in ExportKind::all_names() {
            let parsed = ExportKind::from_str_loose(name);
            prop_assert!(parsed.is_some(),
                "all_names entry '{}' should parse via from_str_loose", name);
        }
    }

    /// Property 10: all_names has exactly 7 entries (one per variant)
    #[test]
    fn prop_export_kind_all_names_count(_dummy in Just(())) {
        prop_assert_eq!(ExportKind::all_names().len(), 7,
            "all_names should have 7 entries, got {}", ExportKind::all_names().len());
    }

    /// Property 11: all_names entries are all unique
    #[test]
    fn prop_export_kind_all_names_unique(_dummy in Just(())) {
        let names = ExportKind::all_names();
        let set: std::collections::HashSet<&&str> = names.iter().collect();
        prop_assert_eq!(set.len(), names.len(),
            "all_names should have unique entries");
    }

    // ========================================================================
    // Property Tests: ExportHeader serialization
    // ========================================================================

    /// Property 12: ExportHeader serializes to valid JSON
    #[test]
    fn prop_export_header_valid_json(header in arb_export_header()) {
        let json = serde_json::to_string(&header).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.is_object(), "Serialized header should be a JSON object");
    }

    /// Property 13: ExportHeader uses _export rename (not "export")
    #[test]
    fn prop_export_header_rename(header in arb_export_header()) {
        let json = serde_json::to_string(&header).unwrap();
        prop_assert!(json.contains("\"_export\""),
            "Should contain _export field, got: {}", json);
        // "export" alone (without underscore prefix) as a key should not appear
        // But we need to be careful: _export contains "export" as substring
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        let map = val.as_object().unwrap();
        prop_assert!(map.contains_key("_export"),
            "Map should have _export key");
        prop_assert!(!map.contains_key("export"),
            "Map should NOT have bare 'export' key");
    }

    /// Property 14: ExportHeader required fields always present
    #[test]
    fn prop_export_header_required_fields(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        let map = val.as_object().unwrap();

        let required = ["_export", "version", "kind", "redacted", "exported_at_ms", "record_count"];
        for field in &required {
            prop_assert!(map.contains_key(*field),
                "Required field '{}' missing from serialized header", field);
        }
    }

    /// Property 15: ExportHeader optional fields skipped when None
    #[test]
    fn prop_export_header_none_fields_skipped(
        version in "0\\.[0-9]+\\.[0-9]+",
        kind in arb_export_kind(),
        record_count in 0..100usize,
    ) {
        let header = ExportHeader {
            export: true,
            version,
            kind: kind.as_str().to_string(),
            redacted: false,
            exported_at_ms: 1_000_000_000_000,
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            record_count,
        };
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        let map = val.as_object().unwrap();

        prop_assert!(!map.contains_key("pane_id"),
            "pane_id=None should be skipped");
        prop_assert!(!map.contains_key("since"),
            "since=None should be skipped");
        prop_assert!(!map.contains_key("until"),
            "until=None should be skipped");
        prop_assert!(!map.contains_key("limit"),
            "limit=None should be skipped");
    }

    /// Property 16: ExportHeader optional fields present when Some
    #[test]
    fn prop_export_header_some_fields_present(
        pane_id in 0..1000u64,
        since in 1_000_000_000_000i64..2_000_000_000_000i64,
        until in 1_000_000_000_000i64..2_000_000_000_000i64,
        limit in 1..10000usize,
    ) {
        let header = ExportHeader {
            export: true,
            version: "0.1.0".to_string(),
            kind: "segments".to_string(),
            redacted: false,
            exported_at_ms: 1_500_000_000_000,
            pane_id: Some(pane_id),
            since: Some(since),
            until: Some(until),
            limit: Some(limit),
            record_count: 0,
        };
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        let map = val.as_object().unwrap();

        prop_assert!(map.contains_key("pane_id"),
            "pane_id=Some should be present");
        prop_assert!(map.contains_key("since"),
            "since=Some should be present");
        prop_assert!(map.contains_key("until"),
            "until=Some should be present");
        prop_assert!(map.contains_key("limit"),
            "limit=Some should be present");
    }

    /// Property 17: ExportHeader preserves version string
    #[test]
    fn prop_export_header_version_preserved(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["version"].as_str().unwrap(), header.version.as_str(),
            "Version should be preserved");
    }

    /// Property 18: ExportHeader preserves kind string
    #[test]
    fn prop_export_header_kind_preserved(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["kind"].as_str().unwrap(), header.kind.as_str(),
            "Kind should be preserved");
    }

    /// Property 19: ExportHeader preserves redacted boolean
    #[test]
    fn prop_export_header_redacted_preserved(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["redacted"].as_bool().unwrap(), header.redacted,
            "Redacted should be preserved");
    }

    /// Property 20: ExportHeader preserves exported_at_ms
    #[test]
    fn prop_export_header_timestamp_preserved(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["exported_at_ms"].as_i64().unwrap(), header.exported_at_ms,
            "exported_at_ms should be preserved");
    }

    /// Property 21: ExportHeader preserves record_count
    #[test]
    fn prop_export_header_record_count_preserved(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["record_count"].as_u64().unwrap(), header.record_count as u64,
            "record_count should be preserved");
    }

    /// Property 22: ExportHeader preserves pane_id when Some
    #[test]
    fn prop_export_header_pane_id_preserved(
        pane_id in 0..10000u64,
    ) {
        let header = ExportHeader {
            export: true,
            version: "0.1.0".to_string(),
            kind: "segments".to_string(),
            redacted: false,
            exported_at_ms: 1_500_000_000_000,
            pane_id: Some(pane_id),
            since: None,
            until: None,
            limit: None,
            record_count: 0,
        };
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["pane_id"].as_u64().unwrap(), pane_id,
            "pane_id should be preserved");
    }

    /// Property 23: ExportHeader _export is always true
    #[test]
    fn prop_export_header_export_always_true(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        prop_assert_eq!(val["_export"].as_bool().unwrap(), true,
            "_export should always be true");
    }

    /// Property 24: ExportHeader JSON field count depends on optional fields
    #[test]
    fn prop_export_header_field_count(header in arb_export_header()) {
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        let map = val.as_object().unwrap();

        let required_count = 6; // _export, version, kind, redacted, exported_at_ms, record_count
        let optional_count = [header.pane_id.is_some(), header.since.is_some(),
                              header.until.is_some(), header.limit.is_some()]
            .iter().filter(|&&b| b).count();

        prop_assert_eq!(map.len(), required_count + optional_count,
            "Field count should be {} + {} = {}, got {}",
            required_count, optional_count, required_count + optional_count, map.len());
    }

    // ========================================================================
    // Property Tests: ExportOptions construction
    // ========================================================================

    /// Property 25: ExportOptions can be constructed with any ExportKind
    #[test]
    fn prop_export_options_construction(kind in arb_export_kind()) {
        let opts = ExportOptions {
            kind,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact: false,
            pretty: false,
        };
        prop_assert_eq!(opts.kind, kind,
            "ExportOptions kind should match");
        prop_assert!(!opts.redact, "Default redact should be false");
        prop_assert!(!opts.pretty, "Default pretty should be false");
    }

    /// Property 26: ExportOptions redact and pretty are independent
    #[test]
    fn prop_export_options_flags_independent(
        kind in arb_export_kind(),
        redact in proptest::bool::ANY,
        pretty in proptest::bool::ANY,
    ) {
        let opts = ExportOptions {
            kind,
            query: ExportQuery::default(),
            audit_actor: None,
            audit_action: None,
            redact,
            pretty,
        };
        prop_assert_eq!(opts.redact, redact, "redact should be preserved");
        prop_assert_eq!(opts.pretty, pretty, "pretty should be preserved");
    }

    /// Property 27: ExportOptions audit_actor and audit_action are preserved
    #[test]
    fn prop_export_options_audit_filters(
        actor in proptest::option::of("[a-z]{3,15}"),
        action in proptest::option::of("[a-z_]{3,15}"),
    ) {
        let opts = ExportOptions {
            kind: ExportKind::Audit,
            query: ExportQuery::default(),
            audit_actor: actor.clone(),
            audit_action: action.clone(),
            redact: false,
            pretty: false,
        };
        prop_assert_eq!(opts.audit_actor, actor, "audit_actor should be preserved");
        prop_assert_eq!(opts.audit_action, action, "audit_action should be preserved");
    }

    // ========================================================================
    // Property Tests: ExportQuery
    // ========================================================================

    /// Property 28: ExportQuery default has all None fields
    #[test]
    fn prop_export_query_default(_dummy in Just(())) {
        let q = ExportQuery::default();
        prop_assert!(q.pane_id.is_none(), "Default pane_id should be None");
        prop_assert!(q.since.is_none(), "Default since should be None");
        prop_assert!(q.until.is_none(), "Default until should be None");
        prop_assert!(q.limit.is_none(), "Default limit should be None");
    }

    /// Property 29: ExportQuery fields are preserved
    #[test]
    fn prop_export_query_fields_preserved(
        pane_id in proptest::option::of(0..1000u64),
        since in proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        until in proptest::option::of(1_000_000_000_000i64..2_000_000_000_000i64),
        limit in proptest::option::of(1..10000usize),
    ) {
        let q = ExportQuery {
            pane_id,
            since,
            until,
            limit,
        };
        prop_assert_eq!(q.pane_id, pane_id, "pane_id should be preserved");
        prop_assert_eq!(q.since, since, "since should be preserved");
        prop_assert_eq!(q.until, until, "until should be preserved");
        prop_assert_eq!(q.limit, limit, "limit should be preserved");
    }

    // ========================================================================
    // Property Tests: Cross-module consistency
    // ========================================================================

    /// Property 30: ExportHeader kind field matches ExportKind::as_str for all variants
    #[test]
    fn prop_header_kind_matches_export_kind(kind in arb_export_kind()) {
        let header = ExportHeader {
            export: true,
            version: "0.1.0".to_string(),
            kind: kind.as_str().to_string(),
            redacted: false,
            exported_at_ms: 1_500_000_000_000,
            pane_id: None,
            since: None,
            until: None,
            limit: None,
            record_count: 0,
        };
        let val: serde_json::Value = serde_json::to_value(&header).unwrap();
        let kind_str = val["kind"].as_str().unwrap();

        // The kind in JSON should round-trip back via from_str_loose
        let parsed = ExportKind::from_str_loose(kind_str);
        prop_assert_eq!(parsed, Some(kind),
            "Header kind '{}' should parse back to {:?}", kind_str, kind);
    }
}
