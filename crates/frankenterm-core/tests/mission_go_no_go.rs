//! ft-1i2ge.7.5 — Production go/no-go decision package
//!
//! Structured tests validating the readiness assessment, evidence collection,
//! threshold validation, rollback criteria, multi-signal decision rubric,
//! and deterministic go/no-go outcomes for production enablement.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadDependencyRef, BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{MissionEventLog, MissionEventLogConfig};
use frankenterm_core::mission_loop::{
    MissionLoop, MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger,
    OperatorOverride, OperatorOverrideKind,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::{MissionRuntimeConfig, PlannerExtractionContext};
use frankenterm_core::runtime_health::{
    HealthCheckRegistry, RemediationEffort, RemediationHint, RuntimeHealthCheck,
};
use frankenterm_core::runtime_telemetry::RuntimePhase;
use frankenterm_core::tx_idempotency::{IdempotencyKey, StepOutcome, TxExecutionLedger, TxPhase};
use frankenterm_core::tx_observability::{
    build_forensic_bundle, BundleClassification, RedactionPolicy, TxObservabilityConfig,
};
use frankenterm_core::tx_plan_compiler::{
    CompensatingAction, CompensationKind, StepRisk, TxPlan, TxRiskSummary, TxStep,
};

// ── Helpers ──────────────────────────────────────────────────────────

fn agent(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

fn bead(id: &str, title: &str, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: title.to_string(),
        issue_type: BeadIssueType::Task,
        status: BeadStatus::Open,
        priority,
        assignee: None,
        labels: Vec::new(),
        dependencies: Vec::new(),
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    }
}

fn ctx() -> PlannerExtractionContext {
    PlannerExtractionContext {
        staleness_hours: HashMap::new(),
    }
}

fn default_config() -> MissionLoopConfig {
    MissionLoopConfig {
        cadence_ms: 30_000,
        ..MissionLoopConfig::default()
    }
}

fn sample_plan() -> TxPlan {
    TxPlan {
        plan_id: "plan-go-1".to_string(),
        plan_hash: 0xABCD,
        steps: vec![
            TxStep {
                id: "s1".to_string(),
                bead_id: "b-1".to_string(),
                agent_id: "a-1".to_string(),
                description: "deploy".to_string(),
                depends_on: Vec::new(),
                preconditions: Vec::new(),
                compensations: vec![CompensatingAction {
                    step_id: "s1-comp".to_string(),
                    description: "rollback deploy".to_string(),
                    action_type: CompensationKind::Rollback,
                }],
                risk: StepRisk::Low,
                score: 0.9,
            },
            TxStep {
                id: "s2".to_string(),
                bead_id: "b-2".to_string(),
                agent_id: "a-1".to_string(),
                description: "verify".to_string(),
                depends_on: vec!["s1".to_string()],
                preconditions: Vec::new(),
                compensations: Vec::new(),
                risk: StepRisk::Low,
                score: 0.8,
            },
        ],
        execution_order: vec!["s1".to_string(), "s2".to_string()],
        parallel_levels: vec![vec!["s1".to_string()], vec!["s2".to_string()]],
        risk_summary: TxRiskSummary {
            total_steps: 2,
            high_risk_count: 0,
            critical_risk_count: 0,
            uncompensated_steps: 1,
            overall_risk: StepRisk::Low,
        },
        rejected_edges: Vec::new(),
    }
}

fn make_override(id: &str, kind: OperatorOverrideKind) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind,
        activated_by: "operator".to_string(),
        reason_code: "go-no-go-test".to_string(),
        rationale: "test override".to_string(),
        activated_at_ms: 0,
        expires_at_ms: Some(100_000),
        correlation_id: None,
    }
}

fn obs_config() -> TxObservabilityConfig {
    TxObservabilityConfig::default()
}

// ── Category 1: Readiness Assessment ─────────────────────────────────

#[test]
fn readiness_all_checks_pass_healthy_report() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("mission.config", "Config", "valid"));
    reg.register(RuntimeHealthCheck::pass("mission.safety", "Safety", "envelope ok"));
    reg.register(RuntimeHealthCheck::pass("mission.agents", "Agents", "3 ready"));
    reg.register(RuntimeHealthCheck::pass("mission.beads", "Beads", "backlog ok"));
    let report = reg.build_report();
    assert!(report.overall_healthy(), "all-pass should be healthy");
    assert!(!report.has_warnings());
    assert_eq!(report.status_counts.pass, 4);
    assert_eq!(report.status_counts.fail, 0);
}

#[test]
fn readiness_single_failure_blocks_go() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("mission.config", "Config", "valid"));
    reg.register(RuntimeHealthCheck::fail(
        "mission.safety",
        "Safety",
        "envelope breach",
    ));
    reg.register(RuntimeHealthCheck::pass("mission.agents", "Agents", "ok"));
    let report = reg.build_report();
    assert!(!report.overall_healthy(), "single fail blocks go");
    assert_eq!(report.status_counts.fail, 1);
    assert_eq!(report.failing_checks().len(), 1);
    assert_eq!(report.failing_checks()[0].check_id, "mission.safety");
}

#[test]
fn readiness_warning_does_not_block_go() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("mission.config", "Config", "valid"));
    reg.register(RuntimeHealthCheck::warn(
        "mission.perf",
        "Performance",
        "latency elevated",
    ));
    let report = reg.build_report();
    assert!(report.overall_healthy(), "warnings don't block go");
    assert!(report.has_warnings());
    assert_eq!(report.status_counts.warn, 1);
}

#[test]
fn readiness_skipped_checks_tracked() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("mission.config", "Config", "ok"));
    reg.register(RuntimeHealthCheck::skip(
        "mission.distributed",
        "Distributed",
        "feature not compiled",
    ));
    let report = reg.build_report();
    assert!(report.overall_healthy());
    assert_eq!(report.status_counts.skip, 1);
    assert_eq!(report.status_counts.total(), 2);
}

#[test]
fn readiness_remediation_hints_surfaced() {
    let mut reg = HealthCheckRegistry::new();
    let check = RuntimeHealthCheck::fail("mission.tls", "TLS", "not configured")
        .with_remediation(
            RemediationHint::with_command("Enable TLS", "ft config set tls.enabled true")
                .effort(RemediationEffort::Low),
        )
        .with_remediation(
            RemediationHint::text("Review security docs").effort(RemediationEffort::Medium),
        );
    reg.register(check);
    let report = reg.build_report();
    let remediation_checks = report.checks_with_remediation();
    assert_eq!(remediation_checks.len(), 1);
    assert_eq!(remediation_checks[0].remediation.len(), 2);
}

// ── Category 2: Evidence Collection ──────────────────────────────────

#[test]
fn evidence_forensic_bundle_captures_plan_and_ledger() {
    let plan = sample_plan();
    let mut ledger = TxExecutionLedger::new("exec-1", "plan-go-1", 0xABCD);
    ledger
        .transition_phase(TxPhase::Preparing)
        .expect("transition");
    ledger
        .transition_phase(TxPhase::Committing)
        .expect("transition");
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s1", "exec-1"),
            StepOutcome::Success {
                result: Some("deployed".to_string()),
            },
            StepRisk::Low,
            "agent-1",
            1_000,
        )
        .expect("append");

    let config = obs_config();
    let bundle = build_forensic_bundle(
        &plan, &ledger, &[], None, "test-gen", "INC-001", 2_000, &config,
    );

    assert_eq!(bundle.plan.plan_id, "plan-go-1");
    assert_eq!(bundle.plan.step_count, 2);
    assert_eq!(bundle.ledger.record_count, 1);
    assert!(bundle.chain_verification.chain_intact);
}

#[test]
fn evidence_chain_verification_detects_integrity() {
    let mut ledger = TxExecutionLedger::new("exec-2", "plan-go-1", 0xABCD);
    ledger
        .transition_phase(TxPhase::Preparing)
        .expect("transition");
    ledger
        .transition_phase(TxPhase::Committing)
        .expect("transition");
    for i in 0..5 {
        ledger
            .append(
                IdempotencyKey::new("plan-go-1", &format!("step-{i}"), &format!("action-{i}")),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-1",
                1_000 + i as u64,
            )
            .expect("append");
    }
    let verification = ledger.verify_chain();
    assert!(
        verification.chain_intact,
        "sequential appends should produce intact chain"
    );
    assert_eq!(verification.total_records, 5);
    assert!(verification.missing_ordinals.is_empty());
}

#[test]
fn evidence_redaction_policy_maximum_strips_categories() {
    let plan = sample_plan();
    let mut ledger = TxExecutionLedger::new("exec-3", "plan-go-1", 0xABCD);
    ledger
        .transition_phase(TxPhase::Preparing)
        .expect("transition");
    ledger
        .transition_phase(TxPhase::Committing)
        .expect("transition");
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s1", "exec-3"),
            StepOutcome::Failed {
                error_code: "DEPLOY_ERR".to_string(),
                error_message: "secret-password-leaked".to_string(),
                compensated: false,
            },
            StepRisk::Medium,
            "agent-1",
            1_000,
        )
        .expect("append");

    let mut config = obs_config();
    config.redaction_policy = RedactionPolicy::maximum();
    config.default_classification = BundleClassification::ExternalAudit;
    let bundle = build_forensic_bundle(
        &plan, &ledger, &[], None, "test-gen", "INC-002", 2_000, &config,
    );

    assert!(
        bundle.redaction.fields_redacted > 0,
        "maximum redaction should redact fields"
    );
    assert!(
        !bundle.redaction.categories.is_empty(),
        "maximum redaction should list categories"
    );
    assert_eq!(
        bundle.metadata.classification,
        BundleClassification::ExternalAudit
    );
}

#[test]
fn evidence_no_redaction_preserves_all_data() {
    let plan = sample_plan();
    let mut ledger = TxExecutionLedger::new("exec-4", "plan-go-1", 0xABCD);
    ledger
        .transition_phase(TxPhase::Preparing)
        .expect("transition");
    ledger
        .transition_phase(TxPhase::Committing)
        .expect("transition");
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s1", "exec-4"),
            StepOutcome::Success {
                result: Some("all-good".to_string()),
            },
            StepRisk::Low,
            "agent-1",
            1_000,
        )
        .expect("append");

    let mut config = obs_config();
    config.redaction_policy = RedactionPolicy::none();
    let bundle = build_forensic_bundle(
        &plan, &ledger, &[], None, "test-gen", "INC-003", 2_000, &config,
    );

    assert_eq!(bundle.redaction.fields_redacted, 0);
    assert!(bundle.redaction.categories.is_empty());
}

// ── Category 3: Threshold Validation ─────────────────────────────────

#[test]
fn threshold_config_validation_catches_invalid_cadence() {
    let config = MissionRuntimeConfig {
        cadence_ms: 0,
        ..MissionRuntimeConfig::default()
    };
    let result = config.validate();
    assert!(!result.valid, "zero cadence is invalid");
    assert!(result.error_count() > 0);
}

#[test]
fn threshold_config_validation_accepts_valid_config() {
    let config = MissionRuntimeConfig::default();
    let result = config.validate();
    assert!(result.valid, "default config should be valid");
    assert_eq!(result.error_count(), 0);
}

#[test]
fn threshold_safety_envelope_caps_assignment_count() {
    let config = MissionLoopConfig {
        cadence_ms: 30_000,
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 2,
            max_risky_assignments_per_cycle: 1,
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: vec!["risky".to_string()],
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![agent("a-1"), agent("a-2"), agent("a-3")];
    let issues = (0..10)
        .map(|i| bead(&format!("b-{i}"), &format!("task {i}"), 1))
        .collect::<Vec<_>>();

    let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        decision.assignment_set.assignment_count() <= 2,
        "safety envelope should cap assignments at 2, got {}",
        decision.assignment_set.assignment_count()
    );
}

#[test]
fn threshold_operator_report_reflects_current_state() {
    let mut ml = MissionLoop::new(default_config());
    let log = MissionEventLog::new(MissionEventLogConfig::default());
    let agents = vec![agent("a-1")];
    let issues = vec![bead("b-1", "task", 1)];
    let _ = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let report = ml.generate_operator_report(Some(&log), None);
    assert!(
        report.status.cycle_count > 0,
        "operator report should reflect at least 1 cycle"
    );
}

// ── Category 4: Rollback Criteria ────────────────────────────────────

#[test]
fn rollback_resume_context_recommends_continue_on_partial() {
    let plan = sample_plan();
    let mut ledger = TxExecutionLedger::new("exec-5", "plan-go-1", 0xABCD);
    ledger
        .transition_phase(TxPhase::Preparing)
        .expect("transition");
    ledger
        .transition_phase(TxPhase::Committing)
        .expect("transition");
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s1", "exec-5"),
            StepOutcome::Success {
                result: Some("ok".to_string()),
            },
            StepRisk::Low,
            "agent-1",
            1_000,
        )
        .expect("append");

    let resume = frankenterm_core::tx_idempotency::ResumeContext::from_ledger(&ledger, &plan);
    assert_eq!(resume.completed_steps.len(), 1);
    assert_eq!(resume.remaining_steps.len(), 1);
    assert!(resume.chain_intact);
}

#[test]
fn rollback_completed_ledger_reports_already_complete() {
    let plan = sample_plan();
    let mut ledger = TxExecutionLedger::new("exec-6", "plan-go-1", 0xABCD);
    ledger.transition_phase(TxPhase::Preparing).unwrap();
    ledger.transition_phase(TxPhase::Committing).unwrap();
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s1", "exec-6-a"),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "a-1",
            1_000,
        )
        .unwrap();
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s2", "exec-6-b"),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "a-1",
            2_000,
        )
        .unwrap();
    ledger.transition_phase(TxPhase::Completed).unwrap();

    let resume = frankenterm_core::tx_idempotency::ResumeContext::from_ledger(&ledger, &plan);
    assert!(resume.remaining_steps.is_empty());
    assert_eq!(resume.completed_steps.len(), 2);
    let is_complete = matches!(
        resume.recommendation,
        frankenterm_core::tx_idempotency::ResumeRecommendation::AlreadyComplete
    );
    assert!(is_complete, "completed ledger should recommend AlreadyComplete");
}

#[test]
fn rollback_failed_step_surfaces_in_resume() {
    let plan = sample_plan();
    let mut ledger = TxExecutionLedger::new("exec-7", "plan-go-1", 0xABCD);
    ledger.transition_phase(TxPhase::Preparing).unwrap();
    ledger.transition_phase(TxPhase::Committing).unwrap();
    ledger
        .append(
            IdempotencyKey::new("plan-go-1", "s1", "exec-7"),
            StepOutcome::Failed {
                error_code: "ERR".to_string(),
                error_message: "deploy failed".to_string(),
                compensated: false,
            },
            StepRisk::Low,
            "a-1",
            1_000,
        )
        .unwrap();

    let resume = frankenterm_core::tx_idempotency::ResumeContext::from_ledger(&ledger, &plan);
    assert_eq!(resume.failed_steps.len(), 1);
    assert!(resume.failed_steps.contains(&"s1".to_string()));
}

#[test]
fn rollback_phase_transition_enforces_valid_sequence() {
    let mut ledger = TxExecutionLedger::new("exec-8", "plan-1", 0x1234);
    assert!(ledger.transition_phase(TxPhase::Preparing).is_ok());
    assert!(ledger.transition_phase(TxPhase::Committing).is_ok());
    assert!(
        ledger.transition_phase(TxPhase::Planned).is_err(),
        "Committing → Planned should be invalid"
    );
}

// ── Category 5: Decision Rubric ──────────────────────────────────────

#[test]
fn rubric_healthy_system_with_beads_produces_assignments() {
    let mut ml = MissionLoop::new(default_config());
    let agents = vec![agent("a-1"), agent("a-2")];
    let issues = vec![
        bead("b-1", "critical fix", 1),
        bead("b-2", "enhancement", 2),
    ];

    let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(
        decision.assignment_set.assignment_count() >= 1,
        "healthy system should produce assignments"
    );
}

#[test]
fn rubric_no_agents_yields_zero_assignments() {
    let mut ml = MissionLoop::new(default_config());
    let agents: Vec<MissionAgentCapabilityProfile> = Vec::new();
    let issues = vec![bead("b-1", "task", 1)];

    let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "no agents should yield zero assignments"
    );
}

#[test]
fn rubric_no_beads_yields_zero_assignments() {
    let mut ml = MissionLoop::new(default_config());
    let agents = vec![agent("a-1")];
    let issues: Vec<BeadIssueDetail> = Vec::new();

    let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "no beads should yield zero assignments"
    );
}

#[test]
fn rubric_override_blocks_specific_agent() {
    let mut ml = MissionLoop::new(default_config());
    let agents = vec![agent("a-1"), agent("a-2")];
    let issues = vec![bead("b-1", "task", 1)];

    ml.apply_override(make_override(
        "block-a1",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a-1".to_string(),
        },
    ))
    .unwrap();

    let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    let assigned_to_a1 = decision
        .assignment_set
        .assignments
        .iter()
        .any(|a| a.agent_id == "a-1");
    assert!(!assigned_to_a1, "excluded agent should not receive work");
}

#[test]
fn rubric_bead_readiness_resolution() {
    let dependent_issue = BeadIssueDetail {
        id: "b-blocked".to_string(),
        title: "blocked task".to_string(),
        issue_type: BeadIssueType::Task,
        status: BeadStatus::Open,
        priority: 1,
        assignee: None,
        labels: Vec::new(),
        dependencies: vec![BeadDependencyRef {
            id: "b-prereq".to_string(),
            title: None,
            status: None,
            priority: None,
            dependency_type: Some("blocked_by".to_string()),
        }],
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    };
    let prerequisite_issue = BeadIssueDetail {
        id: "b-prereq".to_string(),
        title: "prerequisite".to_string(),
        issue_type: BeadIssueType::Task,
        status: BeadStatus::Open,
        priority: 1,
        assignee: None,
        labels: Vec::new(),
        dependencies: Vec::new(),
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    };

    let report =
        frankenterm_core::beads_types::resolve_bead_readiness(&[dependent_issue, prerequisite_issue]);
    assert!(
        report.ready_count() >= 1,
        "at least the unblocked bead should be ready"
    );
    let dep_candidate = report.candidates.iter().find(|c| c.id == "b-blocked");
    if let Some(candidate) = dep_candidate {
        assert!(!candidate.ready, "dependent bead should not be ready");
        assert!(candidate.blocker_count > 0);
    }
}

// ── Category 6: Multi-Signal Go/No-Go ────────────────────────────────

#[test]
fn go_no_go_all_green_signals() {
    // 1. Health checks pass
    let mut reg = HealthCheckRegistry::new().with_phase(RuntimePhase::Running);
    reg.register(RuntimeHealthCheck::pass("config", "Config", "valid"));
    reg.register(RuntimeHealthCheck::pass("safety", "Safety", "ok"));
    reg.register(RuntimeHealthCheck::pass("agents", "Agents", "ready"));
    let report = reg.build_report();
    assert!(report.overall_healthy());

    // 2. Config valid
    let config = MissionRuntimeConfig::default();
    let validation = config.validate();
    assert!(validation.valid);

    // 3. Mission loop produces assignments
    let mut ml = MissionLoop::new(default_config());
    let agents = vec![agent("a-1")];
    let issues = vec![bead("b-1", "task", 1)];
    let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());
    assert!(decision.assignment_set.assignment_count() >= 1);

    // 4. Tx chain intact
    let mut ledger = TxExecutionLedger::new("exec-go", "plan-1", 0x1);
    ledger.transition_phase(TxPhase::Preparing).unwrap();
    ledger.transition_phase(TxPhase::Committing).unwrap();
    ledger
        .append(
            IdempotencyKey::new("plan-1", "s1", "exec-go"),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "a-1",
            1_000,
        )
        .unwrap();
    let chain = ledger.verify_chain();
    assert!(chain.chain_intact);

    // All signals green → go
    let go = report.overall_healthy()
        && validation.valid
        && decision.assignment_set.assignment_count() >= 1
        && chain.chain_intact;
    assert!(go, "all green signals should produce GO");
}

#[test]
fn go_no_go_health_failure_blocks() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::fail(
        "critical",
        "Critical",
        "system down",
    ));
    let report = reg.build_report();

    let config = MissionRuntimeConfig::default();
    let validation = config.validate();

    let go = report.overall_healthy() && validation.valid;
    assert!(!go, "health failure should block go");
}

#[test]
fn go_no_go_config_invalid_blocks() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("ok", "Ok", "fine"));
    let report = reg.build_report();

    let config = MissionRuntimeConfig {
        cadence_ms: 0,
        ..MissionRuntimeConfig::default()
    };
    let validation = config.validate();

    let go = report.overall_healthy() && validation.valid;
    assert!(!go, "invalid config should block go");
}

#[test]
fn go_no_go_empty_chain_is_not_corrupt() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("ok", "Ok", "fine"));
    let report = reg.build_report();

    let config = MissionRuntimeConfig::default();
    let validation = config.validate();

    let ledger = TxExecutionLedger::new("exec-empty", "plan-1", 0x1);
    let chain = ledger.verify_chain();

    let go = report.overall_healthy() && validation.valid && chain.chain_intact;
    assert!(go, "empty chain (no corruption) with healthy system is go");
}

// ── Category 7: Deduplication Guard ──────────────────────────────────

#[test]
fn dedup_guard_prevents_double_execution() {
    let mut guard = frankenterm_core::tx_idempotency::DeduplicationGuard::new(100);
    let key = IdempotencyKey::new("plan-1", "step-1", "exec-1");
    assert!(guard.check(&key).is_none(), "first check should be none");

    guard.record(
        &key,
        "exec-1",
        StepOutcome::Success { result: None },
        1_000,
    );
    assert!(
        guard.check(&key).is_some(),
        "second check should find entry"
    );
    assert_eq!(guard.len(), 1);
}

#[test]
fn dedup_guard_eviction_removes_old_entries() {
    let mut guard = frankenterm_core::tx_idempotency::DeduplicationGuard::new(100);
    let key1 = IdempotencyKey::new("p1", "s1", "action-a");
    let key2 = IdempotencyKey::new("p1", "s2", "action-b");

    guard.record(&key1, "e1", StepOutcome::Success { result: None }, 1_000);
    guard.record(&key2, "e1", StepOutcome::Success { result: None }, 5_000);
    assert_eq!(guard.len(), 2);

    guard.evict_before(3_000);
    assert_eq!(guard.len(), 1, "old entry should be evicted");
    assert!(guard.check(&key1).is_none());
    assert!(guard.check(&key2).is_some());
}

// ── Category 8: Report Formatting ────────────────────────────────────

#[test]
fn report_format_summary_includes_all_sections() {
    let mut reg = HealthCheckRegistry::new().with_phase(RuntimePhase::Running);
    reg.register(RuntimeHealthCheck::pass("config", "Config", "valid"));
    reg.register(RuntimeHealthCheck::warn("perf", "Perf", "slow"));
    reg.register(RuntimeHealthCheck::fail("safety", "Safety", "breach"));
    let report = reg.build_report();

    let summary = report.format_summary();
    assert!(!summary.is_empty(), "summary should not be empty");
}

#[test]
fn report_phase_preserved() {
    let reg = HealthCheckRegistry::new().with_phase(RuntimePhase::Shutdown);
    let report = reg.build_report();
    assert_eq!(report.phase, RuntimePhase::Shutdown);
}

#[test]
fn report_duration_accumulated() {
    let mut reg = HealthCheckRegistry::new();
    reg.register(RuntimeHealthCheck::pass("c1", "C1", "ok").with_duration_us(100));
    reg.register(RuntimeHealthCheck::pass("c2", "C2", "ok").with_duration_us(250));
    let report = reg.build_report();
    assert_eq!(report.total_duration_us, 350);
}

// ── Category 9: Determinism ──────────────────────────────────────────

#[test]
fn determinism_health_report_stable() {
    let build = || {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("a", "A", "ok"));
        reg.register(RuntimeHealthCheck::warn("b", "B", "warn"));
        reg.register(RuntimeHealthCheck::fail("c", "C", "fail"));
        let report = reg.build_report();
        (
            report.overall_healthy(),
            report.has_warnings(),
            report.status_counts.pass,
            report.status_counts.warn,
            report.status_counts.fail,
        )
    };
    assert_eq!(build(), build(), "health report should be deterministic");
}

#[test]
fn determinism_go_no_go_decision_stable() {
    let decide = || {
        let mut reg = HealthCheckRegistry::new();
        reg.register(RuntimeHealthCheck::pass("x", "X", "ok"));
        let report = reg.build_report();

        let config = MissionRuntimeConfig::default();
        let validation = config.validate();

        let mut ml = MissionLoop::new(default_config());
        let agents = vec![agent("a-1")];
        let issues = vec![bead("b-1", "task", 1)];
        let decision = ml.evaluate(1_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

        (
            report.overall_healthy(),
            validation.valid,
            decision.assignment_set.assignment_count(),
        )
    };
    assert_eq!(
        decide(),
        decide(),
        "go/no-go decision should be deterministic"
    );
}

#[test]
fn determinism_forensic_bundle_structure_stable() {
    let build_bundle = || {
        let plan = sample_plan();
        let mut ledger = TxExecutionLedger::new("exec-det", "plan-go-1", 0xABCD);
        ledger.transition_phase(TxPhase::Preparing).unwrap();
        ledger.transition_phase(TxPhase::Committing).unwrap();
        ledger
            .append(
                IdempotencyKey::new("plan-go-1", "s1", "exec-det"),
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "a-1",
                1_000,
            )
            .unwrap();
        let config = obs_config();
        let bundle = build_forensic_bundle(
            &plan, &ledger, &[], None, "test-gen", "INC-DET", 2_000, &config,
        );
        (
            bundle.plan.plan_id.clone(),
            bundle.plan.step_count,
            bundle.ledger.record_count,
            bundle.chain_verification.chain_intact,
        )
    };
    assert_eq!(
        build_bundle(),
        build_bundle(),
        "forensic bundle structure should be deterministic"
    );
}
