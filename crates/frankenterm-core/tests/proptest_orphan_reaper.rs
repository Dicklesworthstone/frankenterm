//! Property-based tests for the orphan_reaper module.
//!
//! Tests structural invariants of ReapReport, including Default stability,
//! Serialize validity, Clone equivalence, and killed/scanned consistency.

use frankenterm_core::orphan_reaper::ReapReport;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

/// Generate a structurally consistent ReapReport where killed_pids.len() == killed
/// and killed <= scanned.
fn arb_consistent_report() -> impl Strategy<Value = ReapReport> {
    (
        0usize..100,                                                            // scanned
        proptest::collection::vec(1u32..65535, 0..20),                          // killed_pids
        proptest::collection::vec("[a-zA-Z0-9 :]{1,40}".prop_map(|s| s), 0..5), // errors
    )
        .prop_map(|(scanned_extra, killed_pids, errors)| {
            let killed = killed_pids.len();
            let scanned = killed + scanned_extra;
            ReapReport {
                scanned,
                killed,
                killed_pids,
                errors,
            }
        })
}

/// Generate an arbitrary ReapReport (possibly inconsistent — killed_pids.len() may
/// differ from killed, and killed may exceed scanned).
fn arb_arbitrary_report() -> impl Strategy<Value = ReapReport> {
    (
        0usize..200,
        0usize..200,
        proptest::collection::vec(1u32..65535, 0..20),
        proptest::collection::vec("[a-zA-Z0-9 :]{1,40}".prop_map(|s| s), 0..5),
    )
        .prop_map(|(scanned, killed, killed_pids, errors)| ReapReport {
            scanned,
            killed,
            killed_pids,
            errors,
        })
}

// ── ReapReport: Default ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default report has zero counts and empty vectors.
    #[test]
    fn report_default_is_empty(_i in 0..1u8) {
        let d = ReapReport::default();
        prop_assert_eq!(d.scanned, 0);
        prop_assert_eq!(d.killed, 0);
        prop_assert!(d.killed_pids.is_empty(), "default killed_pids should be empty");
        prop_assert!(d.errors.is_empty(), "default errors should be empty");
    }

    /// Default is deterministic.
    #[test]
    fn report_default_deterministic(_i in 0..1u8) {
        let a = ReapReport::default();
        let b = ReapReport::default();
        prop_assert_eq!(a.scanned, b.scanned);
        prop_assert_eq!(a.killed, b.killed);
        prop_assert_eq!(a.killed_pids.len(), b.killed_pids.len());
        prop_assert_eq!(a.errors.len(), b.errors.len());
    }
}

// ── ReapReport: Serialize ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Serialized report is valid JSON.
    #[test]
    fn report_serialize_valid_json(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Required fields are present in serialized JSON.
    #[test]
    fn report_serialize_has_required_fields(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("scanned").is_some(), "missing 'scanned'");
        prop_assert!(value.get("killed").is_some(), "missing 'killed'");
        prop_assert!(value.get("killed_pids").is_some(), "missing 'killed_pids'");
        prop_assert!(value.get("errors").is_some(), "missing 'errors'");
    }

    /// Scanned and killed counts are preserved in JSON.
    #[test]
    fn report_serialize_preserves_counts(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let scanned = value.get("scanned").unwrap().as_u64().unwrap() as usize;
        let killed = value.get("killed").unwrap().as_u64().unwrap() as usize;
        prop_assert_eq!(scanned, r.scanned);
        prop_assert_eq!(killed, r.killed);
    }

    /// killed_pids serializes as a JSON array.
    #[test]
    fn report_serialize_pids_is_array(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pids = value.get("killed_pids").unwrap();
        prop_assert!(pids.is_array(), "killed_pids should be a JSON array");
        let arr = pids.as_array().unwrap();
        prop_assert_eq!(arr.len(), r.killed_pids.len(),
            "killed_pids JSON array length mismatch");
    }

    /// Each PID in killed_pids is preserved in JSON.
    #[test]
    fn report_serialize_preserves_pids(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = value.get("killed_pids").unwrap().as_array().unwrap();
        for (i, val) in arr.iter().enumerate() {
            let pid = val.as_u64().unwrap() as u32;
            prop_assert_eq!(pid, r.killed_pids[i],
                "PID mismatch at index {}", i);
        }
    }

    /// Errors serialize as a JSON array of strings.
    #[test]
    fn report_serialize_errors_is_string_array(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let errors = value.get("errors").unwrap().as_array().unwrap();
        for (i, val) in errors.iter().enumerate() {
            prop_assert!(val.is_string(),
                "error at index {} should be a string, got: {}", i, val);
        }
    }

    /// Serialization is deterministic.
    #[test]
    fn report_serialize_deterministic(r in arb_consistent_report()) {
        let j1 = serde_json::to_string(&r).unwrap();
        let j2 = serde_json::to_string(&r).unwrap();
        prop_assert_eq!(j1.as_str(), j2.as_str());
    }

    /// Pretty-printed JSON is also valid.
    #[test]
    fn report_serialize_pretty(r in arb_consistent_report()) {
        let json = serde_json::to_string_pretty(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }
}

// ── ReapReport: structural invariants ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// In a consistent report, killed <= scanned.
    #[test]
    fn report_consistent_killed_le_scanned(r in arb_consistent_report()) {
        prop_assert!(r.killed <= r.scanned,
            "killed ({}) should be <= scanned ({})", r.killed, r.scanned);
    }

    /// In a consistent report, killed_pids.len() == killed.
    #[test]
    fn report_consistent_pids_match_killed(r in arb_consistent_report()) {
        prop_assert_eq!(r.killed_pids.len(), r.killed,
            "killed_pids.len() ({}) should equal killed ({})",
            r.killed_pids.len(), r.killed);
    }

    /// Default report satisfies all consistency invariants.
    #[test]
    fn report_default_is_consistent(_i in 0..1u8) {
        let d = ReapReport::default();
        prop_assert_eq!(d.killed_pids.len(), d.killed);
        prop_assert!(d.killed <= d.scanned);
    }

    /// Consistent reports maintain invariants through JSON serialization.
    #[test]
    fn report_consistency_through_json(r in arb_consistent_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let scanned = value.get("scanned").unwrap().as_u64().unwrap() as usize;
        let killed = value.get("killed").unwrap().as_u64().unwrap() as usize;
        let pids_len = value.get("killed_pids").unwrap().as_array().unwrap().len();
        prop_assert!(killed <= scanned,
            "JSON: killed ({}) should be <= scanned ({})", killed, scanned);
        prop_assert_eq!(pids_len, killed,
            "JSON: killed_pids.len() ({}) should equal killed ({})", pids_len, killed);
    }
}

// ── ReapReport: arbitrary (inconsistent) reports ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Even inconsistent reports serialize to valid JSON without panics.
    #[test]
    fn arbitrary_report_serialize_no_panic(r in arb_arbitrary_report()) {
        let json = serde_json::to_string(&r).unwrap();
        let _: serde_json::Value = serde_json::from_str(&json).unwrap();
    }

    /// Even inconsistent reports can be cloned without panic.
    #[test]
    fn arbitrary_report_clone(r in arb_arbitrary_report()) {
        let cloned: ReapReport = r.clone();
        prop_assert_eq!(cloned.scanned, r.scanned);
        prop_assert_eq!(cloned.killed, r.killed);
    }
}

// ── ReapReport: Clone / Debug ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces equivalent report.
    #[test]
    fn report_clone(r in arb_consistent_report()) {
        let cloned = r.clone();
        prop_assert_eq!(cloned.scanned, r.scanned);
        prop_assert_eq!(cloned.killed, r.killed);
        prop_assert_eq!(cloned.killed_pids, r.killed_pids);
        prop_assert_eq!(cloned.errors, r.errors);
    }

    /// Debug format is non-empty.
    #[test]
    fn report_debug_non_empty(r in arb_consistent_report()) {
        let debug = format!("{:?}", r);
        prop_assert!(!debug.is_empty());
    }

    /// Debug format contains key field names.
    #[test]
    fn report_debug_contains_fields(r in arb_consistent_report()) {
        let debug = format!("{:?}", r);
        prop_assert!(debug.contains("scanned"), "Debug should mention 'scanned'");
        prop_assert!(debug.contains("killed"), "Debug should mention 'killed'");
    }
}
