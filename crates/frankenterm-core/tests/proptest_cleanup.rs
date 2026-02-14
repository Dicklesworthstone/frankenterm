//! Property-based tests for the cleanup module.
//!
//! Tests structural invariants of CleanupTableSummary and CleanupPlan,
//! including Default stability, Serialize validity, Clone equivalence,
//! and aggregate consistency.

use frankenterm_core::cleanup::{CleanupPlan, CleanupTableSummary};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_table_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("events".to_string()),
        Just("output_segments".to_string()),
        Just("audit_actions".to_string()),
        Just("usage_metrics".to_string()),
        Just("notification_history".to_string()),
        Just("workflow_steps".to_string()),
    ]
}

fn arb_table_summary() -> impl Strategy<Value = CleanupTableSummary> {
    (
        arb_table_name(),
        0usize..10_000, // eligible_rows
        0usize..10_000, // deleted_rows
        0u32..3650,     // retention_days
    )
        .prop_map(
            |(table, eligible_rows, deleted_rows, retention_days)| CleanupTableSummary {
                table,
                eligible_rows,
                deleted_rows,
                retention_days,
            },
        )
}

fn arb_cleanup_plan() -> impl Strategy<Value = CleanupPlan> {
    (
        proptest::collection::vec(arb_table_summary(), 0..8),
        any::<bool>(), // dry_run
    )
        .prop_map(|(tables, dry_run)| {
            let total_eligible: usize = tables.iter().map(|t| t.eligible_rows).sum();
            let total_deleted: usize = tables.iter().map(|t| t.deleted_rows).sum();
            CleanupPlan {
                tables,
                total_eligible,
                total_deleted,
                dry_run,
            }
        })
}

/// Plan where totals are intentionally out of sync (for testing).
fn arb_arbitrary_plan() -> impl Strategy<Value = CleanupPlan> {
    (
        proptest::collection::vec(arb_table_summary(), 0..8),
        0usize..100_000,
        0usize..100_000,
        any::<bool>(),
    )
        .prop_map(
            |(tables, total_eligible, total_deleted, dry_run)| CleanupPlan {
                tables,
                total_eligible,
                total_deleted,
                dry_run,
            },
        )
}

// ── CleanupTableSummary: Default ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default table summary has empty table name and zero counts.
    #[test]
    fn table_summary_default_is_empty(_i in 0..1u8) {
        let d = CleanupTableSummary::default();
        prop_assert!(d.table.is_empty(), "default table should be empty");
        prop_assert_eq!(d.eligible_rows, 0);
        prop_assert_eq!(d.deleted_rows, 0);
        prop_assert_eq!(d.retention_days, 0);
    }

    /// Default is deterministic.
    #[test]
    fn table_summary_default_deterministic(_i in 0..1u8) {
        let a = CleanupTableSummary::default();
        let b = CleanupTableSummary::default();
        prop_assert_eq!(a.table.as_str(), b.table.as_str());
        prop_assert_eq!(a.eligible_rows, b.eligible_rows);
        prop_assert_eq!(a.deleted_rows, b.deleted_rows);
        prop_assert_eq!(a.retention_days, b.retention_days);
    }
}

// ── CleanupTableSummary: Serialize ──────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Serialized table summary is valid JSON.
    #[test]
    fn table_summary_serialize_valid_json(s in arb_table_summary()) {
        let json = serde_json::to_string(&s).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Required fields are present in serialized JSON.
    #[test]
    fn table_summary_serialize_has_required_fields(s in arb_table_summary()) {
        let json = serde_json::to_string(&s).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("table").is_some(), "missing 'table' field");
        prop_assert!(value.get("eligible_rows").is_some(), "missing 'eligible_rows' field");
        prop_assert!(value.get("deleted_rows").is_some(), "missing 'deleted_rows' field");
        prop_assert!(value.get("retention_days").is_some(), "missing 'retention_days' field");
    }

    /// Table name is preserved in JSON.
    #[test]
    fn table_summary_serialize_preserves_table(s in arb_table_summary()) {
        let json = serde_json::to_string(&s).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let table_val = value.get("table").unwrap().as_str().unwrap();
        prop_assert_eq!(table_val, s.table.as_str());
    }

    /// Numeric fields are preserved in JSON.
    #[test]
    fn table_summary_serialize_preserves_counts(s in arb_table_summary()) {
        let json = serde_json::to_string(&s).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let eligible = value.get("eligible_rows").unwrap().as_u64().unwrap() as usize;
        let deleted = value.get("deleted_rows").unwrap().as_u64().unwrap() as usize;
        let retention = value.get("retention_days").unwrap().as_u64().unwrap() as u32;
        prop_assert_eq!(eligible, s.eligible_rows);
        prop_assert_eq!(deleted, s.deleted_rows);
        prop_assert_eq!(retention, s.retention_days);
    }

    /// Pretty-printed JSON is also valid.
    #[test]
    fn table_summary_serialize_pretty(s in arb_table_summary()) {
        let json = serde_json::to_string_pretty(&s).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Serialization is deterministic.
    #[test]
    fn table_summary_serialize_deterministic(s in arb_table_summary()) {
        let j1 = serde_json::to_string(&s).unwrap();
        let j2 = serde_json::to_string(&s).unwrap();
        prop_assert_eq!(j1.as_str(), j2.as_str());
    }
}

// ── CleanupTableSummary: Clone / Debug ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces equivalent summary.
    #[test]
    fn table_summary_clone(s in arb_table_summary()) {
        let cloned = s.clone();
        prop_assert_eq!(cloned.table.as_str(), s.table.as_str());
        prop_assert_eq!(cloned.eligible_rows, s.eligible_rows);
        prop_assert_eq!(cloned.deleted_rows, s.deleted_rows);
        prop_assert_eq!(cloned.retention_days, s.retention_days);
    }

    /// Debug format is non-empty.
    #[test]
    fn table_summary_debug_non_empty(s in arb_table_summary()) {
        let debug = format!("{:?}", s);
        prop_assert!(!debug.is_empty());
    }
}

// ── CleanupPlan: Default ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default plan has no tables, zero totals, and dry_run is false.
    #[test]
    fn plan_default_is_empty(_i in 0..1u8) {
        let d = CleanupPlan::default();
        prop_assert!(d.tables.is_empty(), "default tables should be empty");
        prop_assert_eq!(d.total_eligible, 0);
        prop_assert_eq!(d.total_deleted, 0);
        prop_assert!(!d.dry_run, "default dry_run should be false");
    }
}

// ── CleanupPlan: Serialize ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Serialized plan is valid JSON.
    #[test]
    fn plan_serialize_valid_json(plan in arb_cleanup_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Required fields are present.
    #[test]
    fn plan_serialize_has_required_fields(plan in arb_cleanup_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("tables").is_some(), "missing 'tables'");
        prop_assert!(value.get("total_eligible").is_some(), "missing 'total_eligible'");
        prop_assert!(value.get("total_deleted").is_some(), "missing 'total_deleted'");
        prop_assert!(value.get("dry_run").is_some(), "missing 'dry_run'");
    }

    /// Tables field serializes as JSON array.
    #[test]
    fn plan_serialize_tables_is_array(plan in arb_cleanup_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let tables = value.get("tables").unwrap();
        prop_assert!(tables.is_array(), "tables should be a JSON array");
        let arr = tables.as_array().unwrap();
        prop_assert_eq!(arr.len(), plan.tables.len(),
            "JSON array length mismatch");
    }

    /// Dry run boolean is preserved.
    #[test]
    fn plan_serialize_preserves_dry_run(plan in arb_cleanup_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let dry_run = value.get("dry_run").unwrap().as_bool().unwrap();
        prop_assert_eq!(dry_run, plan.dry_run);
    }

    /// Total eligible is preserved.
    #[test]
    fn plan_serialize_preserves_totals(plan in arb_cleanup_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let eligible = value.get("total_eligible").unwrap().as_u64().unwrap() as usize;
        let deleted = value.get("total_deleted").unwrap().as_u64().unwrap() as usize;
        prop_assert_eq!(eligible, plan.total_eligible);
        prop_assert_eq!(deleted, plan.total_deleted);
    }

    /// Pretty-printed JSON is also valid.
    #[test]
    fn plan_serialize_pretty(plan in arb_cleanup_plan()) {
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Serialization is deterministic.
    #[test]
    fn plan_serialize_deterministic(plan in arb_cleanup_plan()) {
        let j1 = serde_json::to_string(&plan).unwrap();
        let j2 = serde_json::to_string(&plan).unwrap();
        prop_assert_eq!(j1.as_str(), j2.as_str());
    }
}

// ── CleanupPlan: structural invariants ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Consistent plan: total_eligible == sum of table eligible_rows.
    #[test]
    fn plan_consistent_eligible(plan in arb_cleanup_plan()) {
        let computed: usize = plan.tables.iter().map(|t| t.eligible_rows).sum();
        prop_assert_eq!(plan.total_eligible, computed,
            "total_eligible should be sum of per-table eligible_rows");
    }

    /// Consistent plan: total_deleted == sum of table deleted_rows.
    #[test]
    fn plan_consistent_deleted(plan in arb_cleanup_plan()) {
        let computed: usize = plan.tables.iter().map(|t| t.deleted_rows).sum();
        prop_assert_eq!(plan.total_deleted, computed,
            "total_deleted should be sum of per-table deleted_rows");
    }

    /// Arbitrary plans: totals can be verified against tables.
    #[test]
    fn plan_arbitrary_totals_in_json(plan in arb_arbitrary_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let eligible = value.get("total_eligible").unwrap().as_u64().unwrap() as usize;
        prop_assert_eq!(eligible, plan.total_eligible);
    }

    /// Each table in the plan has a non-negative retention.
    #[test]
    fn plan_all_tables_have_nonneg_retention(plan in arb_cleanup_plan()) {
        // u32 is always >= 0, but verify the concept through JSON parsing
        let json = serde_json::to_string(&plan).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let tables = value.get("tables").unwrap().as_array().unwrap();
        for table_val in tables {
            let retention = table_val.get("retention_days").unwrap().as_u64().unwrap();
            prop_assert!(retention <= u32::MAX as u64,
                "retention_days should fit in u32");
        }
    }
}

// ── CleanupPlan: Clone / Debug ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Clone produces equivalent plan.
    #[test]
    fn plan_clone(plan in arb_cleanup_plan()) {
        let cloned = plan.clone();
        prop_assert_eq!(cloned.tables.len(), plan.tables.len());
        prop_assert_eq!(cloned.total_eligible, plan.total_eligible);
        prop_assert_eq!(cloned.total_deleted, plan.total_deleted);
        prop_assert_eq!(cloned.dry_run, plan.dry_run);
    }

    /// Debug format is non-empty.
    #[test]
    fn plan_debug_non_empty(plan in arb_cleanup_plan()) {
        let debug = format!("{:?}", plan);
        prop_assert!(!debug.is_empty());
    }

    /// Clone preserves individual table details.
    #[test]
    fn plan_clone_preserves_tables(plan in arb_cleanup_plan()) {
        let cloned = plan.clone();
        for (orig, clone_t) in plan.tables.iter().zip(cloned.tables.iter()) {
            prop_assert_eq!(orig.table.as_str(), clone_t.table.as_str());
            prop_assert_eq!(orig.eligible_rows, clone_t.eligible_rows);
            prop_assert_eq!(orig.deleted_rows, clone_t.deleted_rows);
            prop_assert_eq!(orig.retention_days, clone_t.retention_days);
        }
    }
}
