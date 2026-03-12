//! Property-based tests for the `manifest_dep_eradication` module.
//!
//! Covers serde roundtrips and structural invariants for `DepSection`,
//! `DepCondition`, `ManifestFinding`, `EradicationAction`, `EradicationStep`,
//! `EradicationPlan`, `FeatureAlignment`, and `AlignmentReport`.

use frankenterm_core::dependency_eradication::{ForbiddenRuntime, ViolationSeverity};
use frankenterm_core::manifest_dep_eradication::{
    AlignmentReport, DepCondition, DepSection, EradicationAction, EradicationPlan,
    EradicationStep, FeatureAlignment, ManifestFinding, standard_feature_alignments,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_dep_section() -> impl Strategy<Value = DepSection> {
    prop_oneof![
        Just(DepSection::Dependencies),
        Just(DepSection::DevDependencies),
        Just(DepSection::BuildDependencies),
        Just(DepSection::TargetDependencies),
    ]
}

fn arb_dep_condition() -> impl Strategy<Value = DepCondition> {
    prop_oneof![
        Just(DepCondition::Unconditional),
        "[a-z-]{3,15}".prop_map(DepCondition::FeatureGated),
        "cfg\\([a-z_= \"]{5,30}\\)".prop_map(DepCondition::PlatformConditional),
        "[a-z-]{3,15}".prop_map(DepCondition::DefaultFeature),
    ]
}

fn arb_forbidden_runtime() -> impl Strategy<Value = ForbiddenRuntime> {
    prop_oneof![
        Just(ForbiddenRuntime::Tokio),
        Just(ForbiddenRuntime::Smol),
        Just(ForbiddenRuntime::AsyncIo),
        Just(ForbiddenRuntime::AsyncExecutor),
    ]
}

fn arb_violation_severity() -> impl Strategy<Value = ViolationSeverity> {
    prop_oneof![
        Just(ViolationSeverity::Info),
        Just(ViolationSeverity::Warning),
        Just(ViolationSeverity::Error),
        Just(ViolationSeverity::Critical),
    ]
}

fn arb_eradication_action() -> impl Strategy<Value = EradicationAction> {
    prop_oneof![
        Just(EradicationAction::Remove),
        Just(EradicationAction::FeatureGate),
        Just(EradicationAction::MigrateToAsupersync),
        Just(EradicationAction::MoveToDevOnly),
        Just(EradicationAction::AcceptAsVendored),
    ]
}

fn arb_manifest_finding() -> impl Strategy<Value = ManifestFinding> {
    (
        "[a-z-]{3,20}",
        "[a-z-/]{5,30}/Cargo\\.toml",
        "[a-z-]{3,15}",
        arb_forbidden_runtime(),
        arb_dep_section(),
        arb_dep_condition(),
        proptest::collection::vec("[a-z-]{2,10}", 0..3),
        arb_violation_severity(),
    )
        .prop_map(
            |(crate_name, manifest_path, dep_name, runtime, section, condition, features, severity)| {
                ManifestFinding {
                    crate_name,
                    manifest_path,
                    dep_name,
                    runtime,
                    section,
                    condition,
                    features_enabled: features,
                    severity,
                }
            },
        )
}

fn arb_eradication_step() -> impl Strategy<Value = EradicationStep> {
    (
        arb_manifest_finding(),
        arb_eradication_action(),
        "[a-z ]{5,40}",
        proptest::option::of("[a-z-]{3,15}"),
        any::<bool>(),
    )
        .prop_map(|(finding, action, rationale, migration_feature, completed)| EradicationStep {
            finding,
            action,
            rationale,
            migration_feature,
            completed,
        })
}

fn arb_eradication_plan() -> impl Strategy<Value = EradicationPlan> {
    (
        "[a-z-]{5,20}",
        any::<u64>(),
        proptest::collection::vec(arb_eradication_step(), 0..5),
    )
        .prop_map(|(plan_id, ts, steps)| EradicationPlan {
            plan_id,
            generated_at_ms: ts,
            steps,
        })
}

fn arb_feature_alignment() -> impl Strategy<Value = FeatureAlignment> {
    (
        "[a-z-]{3,15}",
        "[a-z-]{3,15}",
        "[a-z-]{3,15}",
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(crate_name, legacy, migration, le, me, dil, aligned)| FeatureAlignment {
                crate_name,
                legacy_feature: legacy,
                migration_feature: migration,
                legacy_exists: le,
                migration_exists: me,
                default_is_legacy: dil,
                aligned,
            },
        )
}

// =========================================================================
// DepSection serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn dep_section_serde_roundtrip(section in arb_dep_section()) {
        let json = serde_json::to_string(&section).unwrap();
        let back: DepSection = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, section);
    }
}

// =========================================================================
// DepCondition serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn dep_condition_serde_roundtrip(cond in arb_dep_condition()) {
        let json = serde_json::to_string(&cond).unwrap();
        let back: DepCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, cond);
    }
}

// =========================================================================
// EradicationAction serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn eradication_action_serde_roundtrip(action in arb_eradication_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: EradicationAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, action);
    }
}

// =========================================================================
// ManifestFinding serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn manifest_finding_serde_roundtrip(finding in arb_manifest_finding()) {
        let json = serde_json::to_string(&finding).unwrap();
        let back: ManifestFinding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.crate_name, &finding.crate_name);
        prop_assert_eq!(&back.manifest_path, &finding.manifest_path);
        prop_assert_eq!(&back.dep_name, &finding.dep_name);
        prop_assert_eq!(back.section, finding.section);
        prop_assert_eq!(&back.condition, &finding.condition);
        prop_assert_eq!(&back.features_enabled, &finding.features_enabled);
    }
}

// =========================================================================
// EradicationStep serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn eradication_step_serde_roundtrip(step in arb_eradication_step()) {
        let json = serde_json::to_string(&step).unwrap();
        let back: EradicationStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.action, step.action);
        prop_assert_eq!(&back.rationale, &step.rationale);
        prop_assert_eq!(&back.migration_feature, &step.migration_feature);
        prop_assert_eq!(back.completed, step.completed);
        prop_assert_eq!(&back.finding.crate_name, &step.finding.crate_name);
    }
}

// =========================================================================
// EradicationPlan serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn eradication_plan_serde_roundtrip(plan in arb_eradication_plan()) {
        let json = serde_json::to_string(&plan).unwrap();
        let back: EradicationPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.plan_id, &plan.plan_id);
        prop_assert_eq!(back.generated_at_ms, plan.generated_at_ms);
        prop_assert_eq!(back.steps.len(), plan.steps.len());
    }
}

// =========================================================================
// FeatureAlignment serde roundtrip
// =========================================================================

proptest! {
    #[test]
    fn feature_alignment_serde_roundtrip(fa in arb_feature_alignment()) {
        let json = serde_json::to_string(&fa).unwrap();
        let back: FeatureAlignment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.crate_name, &fa.crate_name);
        prop_assert_eq!(&back.legacy_feature, &fa.legacy_feature);
        prop_assert_eq!(&back.migration_feature, &fa.migration_feature);
        prop_assert_eq!(back.legacy_exists, fa.legacy_exists);
        prop_assert_eq!(back.migration_exists, fa.migration_exists);
        prop_assert_eq!(back.default_is_legacy, fa.default_is_legacy);
        prop_assert_eq!(back.aligned, fa.aligned);
    }
}

// =========================================================================
// Standard plan structural invariants
// =========================================================================

#[test]
fn standard_plan_has_18_steps() {
    let plan = EradicationPlan::standard();
    assert_eq!(plan.total_steps(), 18);
}

#[test]
fn standard_plan_no_steps_completed() {
    let plan = EradicationPlan::standard();
    assert_eq!(plan.completed_steps(), 0);
}

#[test]
fn standard_plan_has_critical_remaining() {
    let plan = EradicationPlan::standard();
    let critical = plan.critical_remaining();
    assert!(!critical.is_empty(), "standard plan should have critical steps");
}

#[test]
fn standard_plan_covers_all_runtimes() {
    let plan = EradicationPlan::standard();
    let by_runtime = plan.findings_by_runtime();
    assert!(by_runtime.contains_key("tokio"), "should cover tokio");
    assert!(by_runtime.contains_key("smol"), "should cover smol");
    assert!(by_runtime.contains_key("async-io"), "should cover async-io");
    assert!(by_runtime.contains_key("async-executor"), "should cover async-executor");
}

// =========================================================================
// Plan progress invariants
// =========================================================================

proptest! {
    #[test]
    fn plan_progress_pct_in_range(plan in arb_eradication_plan()) {
        let pct = plan.progress_pct();
        prop_assert!(pct >= 0.0);
        prop_assert!(pct <= 100.0);
    }

    #[test]
    fn plan_completed_lte_total(plan in arb_eradication_plan()) {
        prop_assert!(plan.completed_steps() <= plan.total_steps());
    }

    #[test]
    fn plan_by_crate_total_matches(plan in arb_eradication_plan()) {
        let by_crate = plan.by_crate();
        let total: usize = by_crate.values().map(|v| v.len()).sum();
        prop_assert_eq!(total, plan.total_steps());
    }

    #[test]
    fn plan_by_action_total_matches(plan in arb_eradication_plan()) {
        let by_action = plan.by_action();
        let total: usize = by_action.values().map(|v| v.len()).sum();
        prop_assert_eq!(total, plan.total_steps());
    }
}

// =========================================================================
// Standard feature alignments
// =========================================================================

#[test]
fn standard_feature_alignments_has_7() {
    let alignments = standard_feature_alignments();
    assert_eq!(alignments.len(), 7);
}

#[test]
fn standard_feature_alignments_none_aligned() {
    // All standard alignments should be not-yet-aligned (migration incomplete)
    for fa in &standard_feature_alignments() {
        assert!(!fa.aligned, "{} should not be aligned yet", fa.crate_name);
    }
}

// =========================================================================
// AlignmentReport
// =========================================================================

#[test]
fn alignment_report_new_defaults() {
    let report = AlignmentReport::new("test", 42);
    assert_eq!(report.report_id, "test");
    assert_eq!(report.generated_at_ms, 42);
    assert!(!report.overall_aligned);
    assert!((report.readiness_score - 0.0).abs() < f64::EPSILON);
}
