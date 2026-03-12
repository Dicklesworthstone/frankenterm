//! Property tests for ntm_decommission module (ft-3681t.8.5).
//!
//! Covers serde roundtrips, dependency status logic, component category
//! labels, documentation index coverage arithmetic, decommission phase
//! gate pass rates, reversibility window logic, plan migration counters,
//! snapshot consistency, and standard factory invariants.

use frankenterm_core::ntm_decommission::*;
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_dependency_status() -> impl Strategy<Value = DependencyStatus> {
    prop_oneof![
        Just(DependencyStatus::Active),
        Just(DependencyStatus::Migrating),
        Just(DependencyStatus::Migrated),
        Just(DependencyStatus::Retired),
    ]
}

fn arb_component_category() -> impl Strategy<Value = NtmComponentCategory> {
    prop_oneof![
        Just(NtmComponentCategory::SessionManagement),
        Just(NtmComponentCategory::PaneOrchestration),
        Just(NtmComponentCategory::SwarmLauncher),
        Just(NtmComponentCategory::Configuration),
        Just(NtmComponentCategory::RemoteAccess),
        Just(NtmComponentCategory::Monitoring),
        Just(NtmComponentCategory::CliSurface),
    ]
}

fn arb_doc_category() -> impl Strategy<Value = DocCategory> {
    prop_oneof![
        Just(DocCategory::OperatorPlaybook),
        Just(DocCategory::ContributorGuide),
        Just(DocCategory::Adr),
        Just(DocCategory::Runbook),
        Just(DocCategory::ApiReference),
    ]
}

fn _arb_gate_check(passed: bool) -> impl Strategy<Value = GateCheck> {
    ("[A-Z]-[0-9]{1,4}", ".{1,30}").prop_map(move |(id, desc)| GateCheck {
        check_id: id,
        description: desc,
        passed,
        evidence: if passed {
            "verified".to_string()
        } else {
            "not ready".to_string()
        },
    })
}

fn _arb_doc_entry(complete: bool) -> impl Strategy<Value = DocEntry> {
    (
        "[A-Z]-[0-9]{1,4}",
        ".{1,30}",
        arb_doc_category(),
        "[a-z-]{3,15}",
    )
        .prop_map(move |(id, title, cat, topic)| DocEntry {
            doc_id: id,
            title,
            category: cat,
            path: "docs/test.md".to_string(),
            complete,
            topics: vec![topic],
        })
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #[test]
    fn serde_roundtrip_dependency_status(status in arb_dependency_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: DependencyStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back);
    }

    #[test]
    fn serde_roundtrip_component_category(cat in arb_component_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: NtmComponentCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_doc_category(cat in arb_doc_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: DocCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    #[test]
    fn serde_roundtrip_gate_check(passed in any::<bool>()) {
        let gate = GateCheck {
            check_id: "G-1".into(),
            description: "test".into(),
            passed,
            evidence: "ev".into(),
        };
        let json = serde_json::to_string(&gate).unwrap();
        let back: GateCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(gate.check_id, back.check_id);
        prop_assert_eq!(gate.passed, back.passed);
    }

    #[test]
    fn serde_roundtrip_decommission_snapshot(
        total in 0..20usize,
        migrated in 0..20usize,
        active in 0..20usize,
        completed in 0..10usize,
        total_phases in 0..10usize,
        rate in 0.0..1.0f64,
        ready in any::<bool>(),
        entries in 0..50usize,
    ) {
        let snap = DecommissionSnapshot {
            total_dependencies: total,
            migrated_dependencies: migrated,
            active_dependencies: active,
            completed_phases: completed,
            total_phases,
            doc_coverage_rate: rate,
            missing_topics: Vec::new(),
            ready,
            audit_entries: entries,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: DecommissionSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.total_dependencies, back.total_dependencies);
        prop_assert_eq!(snap.migrated_dependencies, back.migrated_dependencies);
        prop_assert_eq!(snap.ready, back.ready);
        prop_assert_eq!(snap.audit_entries, back.audit_entries);
    }
}

// =============================================================================
// DependencyStatus invariants
// =============================================================================

proptest! {
    #[test]
    fn dependency_status_label_nonempty(status in arb_dependency_status()) {
        prop_assert!(!status.label().is_empty());
    }

    #[test]
    fn dependency_status_is_active_correct(status in arb_dependency_status()) {
        let expected = matches!(status, DependencyStatus::Active | DependencyStatus::Migrating);
        prop_assert_eq!(status.is_active(), expected);
    }
}

// =============================================================================
// NtmComponentCategory invariants
// =============================================================================

proptest! {
    #[test]
    fn component_category_label_nonempty(cat in arb_component_category()) {
        prop_assert!(!cat.label().is_empty());
    }
}

// =============================================================================
// DocCategory invariants
// =============================================================================

proptest! {
    #[test]
    fn doc_category_label_nonempty(cat in arb_doc_category()) {
        prop_assert!(!cat.label().is_empty());
    }
}

// =============================================================================
// NtmDependency constructor invariants
// =============================================================================

proptest! {
    #[test]
    fn active_dependency_has_no_target(
        id in "[a-z-]{3,10}",
        desc in ".{1,30}",
        cat in arb_component_category(),
    ) {
        let dep = NtmDependency::active(&id, &desc, cat);
        prop_assert_eq!(dep.status, DependencyStatus::Active);
        prop_assert!(dep.migration_target.is_none());
        prop_assert!(dep.migration_evidence.is_none());
    }

    #[test]
    fn migrated_dependency_has_target(
        id in "[a-z-]{3,10}",
        desc in ".{1,30}",
        cat in arb_component_category(),
        target in ".{1,20}",
        evidence in ".{1,30}",
    ) {
        let dep = NtmDependency::migrated(&id, &desc, cat, &target, &evidence);
        prop_assert_eq!(dep.status, DependencyStatus::Migrated);
        prop_assert!(dep.migration_target.is_some());
        prop_assert!(dep.migration_evidence.is_some());
    }
}

// =============================================================================
// DecommissionPhase gate pass rate
// =============================================================================

proptest! {
    #[test]
    fn gate_pass_rate_arithmetic(
        n_pass in 0..10usize,
        n_fail in 0..10usize,
    ) {
        let total = n_pass + n_fail;
        if total == 0 {
            return Ok(());
        }

        let mut gates = Vec::new();
        for i in 0..n_pass {
            gates.push(GateCheck {
                check_id: format!("P-{i}"),
                description: "pass".into(),
                passed: true,
                evidence: "ok".into(),
            });
        }
        for i in 0..n_fail {
            gates.push(GateCheck {
                check_id: format!("F-{i}"),
                description: "fail".into(),
                passed: false,
                evidence: "not ok".into(),
            });
        }

        let phase = DecommissionPhase {
            phase_id: "test".into(),
            name: "test phase".into(),
            description: "test".into(),
            order: 0,
            gates,
            rollback: RollbackPlan {
                steps: vec!["rollback".into()],
                estimated_time_ms: 1000,
                rehearsed: false,
            },
            complete: false,
        };

        let rate = phase.gate_pass_rate();
        let expected = n_pass as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10,
            "gate_pass_rate {} != expected {}", rate, expected);

        if n_fail == 0 {
            prop_assert!(phase.gates_pass(), "all gates passed but gates_pass() returned false");
        } else {
            prop_assert!(!phase.gates_pass(), "some gates failed but gates_pass() returned true");
        }
    }

    #[test]
    fn empty_gates_rate_is_zero(_dummy in 0..1u32) {
        let phase = DecommissionPhase {
            phase_id: "test".into(),
            name: "test".into(),
            description: "test".into(),
            order: 0,
            gates: Vec::new(),
            rollback: RollbackPlan {
                steps: Vec::new(),
                estimated_time_ms: 0,
                rehearsed: false,
            },
            complete: false,
        };
        prop_assert_eq!(phase.gate_pass_rate(), 0.0);
        prop_assert!(!phase.gates_pass());
    }
}

// =============================================================================
// DocumentationIndex coverage arithmetic
// =============================================================================

proptest! {
    #[test]
    fn coverage_rate_bounds(
        n_complete in 0..10usize,
        n_incomplete in 0..10usize,
    ) {
        let total = n_complete + n_incomplete;
        if total == 0 {
            return Ok(());
        }

        let mut index = DocumentationIndex::new();
        for i in 0..n_complete {
            index.add(DocEntry {
                doc_id: format!("C-{i}"),
                title: "complete".into(),
                category: DocCategory::Runbook,
                path: "docs/test.md".into(),
                complete: true,
                topics: vec![format!("topic-c-{i}")],
            });
        }
        for i in 0..n_incomplete {
            index.add(DocEntry {
                doc_id: format!("I-{i}"),
                title: "incomplete".into(),
                category: DocCategory::Runbook,
                path: "docs/test.md".into(),
                complete: false,
                topics: vec![format!("topic-i-{i}")],
            });
        }

        let rate = index.coverage_rate();
        let expected = n_complete as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10);
        prop_assert!(rate >= 0.0 && rate <= 1.0);
    }

    #[test]
    fn missing_topics_subset_of_required(
        n_required in 1..5usize,
        n_documented in 0..5usize,
    ) {
        let mut index = DocumentationIndex::new();
        for i in 0..n_required {
            index.require_topic(format!("topic-{i}"));
        }
        for i in 0..n_documented.min(n_required) {
            index.add(DocEntry {
                doc_id: format!("D-{i}"),
                title: "doc".into(),
                category: DocCategory::ContributorGuide,
                path: "docs/test.md".into(),
                complete: true,
                topics: vec![format!("topic-{i}")],
            });
        }

        let missing = index.missing_topics();
        let documented_count = n_documented.min(n_required);
        let expected_missing = n_required - documented_count;
        prop_assert_eq!(missing.len(), expected_missing,
            "expected {} missing topics, got {}", expected_missing, missing.len());
    }

    #[test]
    fn all_topics_covered_when_all_documented(
        n_topics in 1..5usize,
    ) {
        let mut index = DocumentationIndex::new();
        for i in 0..n_topics {
            let topic = format!("topic-{i}");
            index.require_topic(&topic);
            index.add(DocEntry {
                doc_id: format!("D-{i}"),
                title: "doc".into(),
                category: DocCategory::OperatorPlaybook,
                path: "docs/test.md".into(),
                complete: true,
                topics: vec![topic],
            });
        }
        prop_assert!(index.all_topics_covered());
    }
}

// =============================================================================
// ReversibilityPolicy window logic
// =============================================================================

proptest! {
    #[test]
    fn within_window_correct(
        decom_at in 0..1_000_000u64,
        elapsed in 0..100_000_000u64,
        window in 1..100_000_000u64,
    ) {
        let policy = ReversibilityPolicy {
            reversal_allowed: true,
            conditions: Vec::new(),
            authorized_by: Vec::new(),
            reversal_window_ms: window,
        };
        let now = decom_at + elapsed;
        let expected = elapsed <= window;
        prop_assert_eq!(policy.within_window(decom_at, now), expected,
            "elapsed={}, window={}", elapsed, window);
    }

    #[test]
    fn disabled_reversal_never_within_window(
        decom_at in 0..1_000_000u64,
        elapsed in 0..100u64,
    ) {
        let policy = ReversibilityPolicy {
            reversal_allowed: false,
            conditions: Vec::new(),
            authorized_by: Vec::new(),
            reversal_window_ms: 1_000_000,
        };
        let now = decom_at + elapsed;
        prop_assert!(!policy.within_window(decom_at, now));
    }
}

// =============================================================================
// DecommissionPlan migration counters
// =============================================================================

proptest! {
    #[test]
    fn migration_counters_consistent(
        n_active in 0..5usize,
        n_migrating in 0..5usize,
        n_migrated in 0..5usize,
        n_retired in 0..5usize,
    ) {
        let total = n_active + n_migrating + n_migrated + n_retired;
        if total == 0 {
            return Ok(());
        }

        let mut deps = Vec::new();
        for i in 0..n_active {
            deps.push(NtmDependency::active(
                format!("A-{i}"), "active dep", NtmComponentCategory::Monitoring));
        }
        for i in 0..n_migrating {
            let mut dep = NtmDependency::active(
                format!("M-{i}"), "migrating dep", NtmComponentCategory::CliSurface);
            dep.status = DependencyStatus::Migrating;
            deps.push(dep);
        }
        for i in 0..n_migrated {
            deps.push(NtmDependency::migrated(
                format!("D-{i}"), "migrated dep",
                NtmComponentCategory::Configuration, "target", "evidence"));
        }
        for i in 0..n_retired {
            let mut dep = NtmDependency::active(
                format!("R-{i}"), "retired dep", NtmComponentCategory::RemoteAccess);
            dep.status = DependencyStatus::Retired;
            deps.push(dep);
        }

        let mut plan = DecommissionPlan::standard();
        plan.dependencies = deps;

        let active_count = plan.active_dependency_count();
        let migrated_count = plan.migrated_dependency_count();

        prop_assert_eq!(active_count, n_active + n_migrating,
            "active count should be active + migrating");
        prop_assert_eq!(migrated_count, n_migrated + n_retired,
            "migrated count should be migrated + retired");
        prop_assert_eq!(active_count + migrated_count, total);

        let rate = plan.migration_rate();
        let expected = migrated_count as f64 / total as f64;
        prop_assert!((rate - expected).abs() < 1e-10);
    }

    #[test]
    fn plan_not_ready_with_active_deps(
        n_active in 1..5usize,
    ) {
        let mut plan = DecommissionPlan::standard();
        for i in 0..n_active {
            plan.dependencies.push(NtmDependency::active(
                format!("EXTRA-{i}"), "extra", NtmComponentCategory::Monitoring));
        }
        prop_assert!(!plan.is_ready(),
            "plan should not be ready with active dependencies");
    }
}

// =============================================================================
// Snapshot consistency
// =============================================================================

proptest! {
    #[test]
    fn snapshot_matches_plan(
        n_audits in 0..10usize,
    ) {
        let mut plan = DecommissionPlan::standard();
        for i in 0..n_audits {
            plan.record_audit(AuditEntry {
                timestamp_ms: i as u64 * 1000,
                action: format!("action-{i}"),
                actor: "test".into(),
                phase_id: None,
                previous_state: None,
                new_state: None,
                notes: None,
            });
        }

        let snap = plan.snapshot();
        prop_assert_eq!(snap.total_dependencies, plan.dependencies.len());
        prop_assert_eq!(snap.migrated_dependencies, plan.migrated_dependency_count());
        prop_assert_eq!(snap.active_dependencies, plan.active_dependency_count());
        prop_assert_eq!(snap.completed_phases, plan.completed_phases());
        prop_assert_eq!(snap.total_phases, plan.phases.len());
        prop_assert_eq!(snap.audit_entries, n_audits);
        prop_assert_eq!(snap.ready, plan.is_ready());
    }
}

// =============================================================================
// Standard factory invariants
// =============================================================================

#[test]
fn standard_dependencies_all_migrated() {
    let deps = standard_ntm_dependencies();
    assert!(deps.len() >= 7);
    for dep in &deps {
        assert_eq!(dep.status, DependencyStatus::Migrated);
        assert!(dep.migration_target.is_some());
    }
}

#[test]
fn standard_phases_are_ordered() {
    let phases = standard_decommission_phases();
    assert!(phases.len() >= 4);
    for w in phases.windows(2) {
        assert!(w[0].order < w[1].order);
    }
}

#[test]
fn standard_docs_cover_all_topics() {
    let index = standard_documentation_index();
    assert!(index.all_topics_covered());
    assert_eq!(index.coverage_rate(), 1.0);
}

#[test]
fn standard_plan_is_ready() {
    let plan = DecommissionPlan::standard();
    assert_eq!(plan.active_dependency_count(), 0);
    assert_eq!(plan.migration_rate(), 1.0);
    assert!(plan.documentation.all_topics_covered());
    assert!(plan.is_ready());
}

#[test]
fn standard_plan_summary_renders() {
    let plan = DecommissionPlan::standard();
    let summary = plan.render_summary();
    assert!(!summary.is_empty());
    assert!(summary.contains("NTM-DECOM-001"));
}

#[test]
fn standard_plan_serde_roundtrip() {
    let plan = DecommissionPlan::standard();
    let json = serde_json::to_string(&plan).unwrap();
    let back: DecommissionPlan = serde_json::from_str(&json).unwrap();
    assert_eq!(back.plan_id, plan.plan_id);
    assert_eq!(back.dependencies.len(), plan.dependencies.len());
    assert_eq!(back.phases.len(), plan.phases.len());
}

#[test]
fn standard_reversibility_allows_reversal() {
    let policy = ReversibilityPolicy::standard();
    assert!(policy.reversal_allowed);
    assert!(!policy.conditions.is_empty());
    assert!(policy.reversal_window_ms > 0);
}
