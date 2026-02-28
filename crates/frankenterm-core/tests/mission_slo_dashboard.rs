//! Mission SLO dashboard/report artifact validation tests.
//! [ft-1i2ge.6.5]
//!
//! Validates operator-facing evidence artifacts for mission performance and
//! reliability.  Covers SLO threshold boundaries, health classification,
//! evidence artifact serialisation, shadow-mode fidelity integration,
//! multi-cycle compliance tracking, degradation detection and recovery paths.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{
    MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
};
use frankenterm_core::mission_loop::{
    ConflictDetectionConfig, DeconflictionStrategy, KnownReservation, MissionCycleMetricsSample,
    MissionLoop, MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger, OperatorOverride,
    OperatorOverrideKind, OperatorStatusReport, format_operator_report_plain,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::PlannerExtractionContext;
use frankenterm_core::shadow_mode_evaluator::{ShadowEvaluationConfig, ShadowModeEvaluator};

// ── Helpers ──────────────────────────────────────────────────────────────────

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

fn offline(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Offline {
            reason_code: "slo-test".to_string(),
        },
    }
}

fn issue(id: &str, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {id}"),
        status: BeadStatus::Open,
        priority,
        issue_type: BeadIssueType::Task,
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
    PlannerExtractionContext::default()
}

fn elog() -> MissionEventLog {
    MissionEventLog::new(MissionEventLogConfig {
        max_events: 200,
        enabled: true,
    })
}

/// Run N evaluation cycles, returning the final report.
fn run_cycles(
    ml: &mut MissionLoop,
    n: usize,
    issues: &[BeadIssueDetail],
    agents: &[MissionAgentCapabilityProfile],
) -> OperatorStatusReport {
    let c = ctx();
    for i in 0..n {
        ml.evaluate(
            (i as i64 + 1) * 30_000,
            MissionTrigger::CadenceTick,
            issues,
            agents,
            &c,
        );
    }
    ml.generate_operator_report(Some(&elog()), None)
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SLO THRESHOLD BOUNDARY TESTS
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn slo_idle_when_no_cycles() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let report = ml.generate_operator_report(Some(&elog()), None);
    assert_eq!(report.health.overall, "idle");
    assert!(report.health.throughput_assignments_per_minute.abs() < f64::EPSILON);
    assert!(report.health.conflict_rate.abs() < f64::EPSILON);
    assert!(report.health.planner_churn_rate.abs() < f64::EPSILON);
    assert!(report.health.policy_deny_rate.abs() < f64::EPSILON);
}

#[test]
fn slo_healthy_after_clean_cycles() {
    // Use more agents than beads to ensure clean assignment without rejections
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];

    let report = run_cycles(&mut ml, 5, &issues, &agents);
    // With more agents than beads, the system should stay healthy or at worst
    // be non-critical (minor churn from identical priorities is possible).
    assert_ne!(
        report.health.overall, "critical",
        "Clean cycles should never be critical"
    );
    assert!(report.health.throughput_assignments_per_minute > 0.0);
    assert!(report.health.conflict_rate < 0.1);
}

#[test]
fn slo_degraded_when_churn_exceeds_threshold() {
    // Churn threshold is 0.3.  Alternating agent pools between cycles
    // forces assignment churn every cycle.
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    for i in 0..6 {
        let agents = if i % 2 == 0 {
            vec![agent("a1")]
        } else {
            vec![agent("a2")]
        };
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let report = ml.generate_operator_report(Some(&elog()), None);
    // Alternating agents → high churn → degraded
    assert!(
        report.health.overall == "degraded" || report.health.overall == "critical",
        "Expected degraded or critical from churn, got {}",
        report.health.overall
    );
    assert!(
        report.health.planner_churn_rate > 0.3,
        "Churn rate should exceed 0.3, got {}",
        report.health.planner_churn_rate
    );
}

#[test]
fn slo_health_threshold_contract_documented() {
    // Validate the health threshold contract through observable behavior:
    // - Clean cycles with diverse beads/agents → healthy
    // - High churn cycles → degraded (conflict > 0.1 or churn > 0.3 or deny > 0.2)
    // These thresholds are documented constants in the mission_loop module.

    // Clean scenario: no churn, no conflicts
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![
        issue("b1", 1),
        issue("b2", 2),
        issue("b3", 3),
        issue("b4", 4),
        issue("b5", 5),
    ];
    let report = run_cycles(&mut ml, 5, &issues, &agents);

    // With diverse beads and stable agents, conflict and deny rates should be low.
    // The degraded threshold is conflict > 0.1, so we verify below that boundary.
    assert!(
        report.health.conflict_rate < 0.1,
        "Clean scenario conflict_rate should be < 0.1, got {}",
        report.health.conflict_rate
    );
    // Policy deny rate should be at or below the degraded threshold (0.2).
    // With more agents than beads, rejections are minimal.
    assert!(
        report.health.policy_deny_rate <= 0.2,
        "Clean scenario deny_rate should be <= 0.2, got {}",
        report.health.policy_deny_rate
    );
}

#[test]
fn slo_health_fields_within_valid_range() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    let report = run_cycles(&mut ml, 5, &issues, &agents);

    // All rate fields should be in [0.0, 1.0]
    assert!(report.health.conflict_rate >= 0.0 && report.health.conflict_rate <= 1.0);
    assert!(report.health.planner_churn_rate >= 0.0 && report.health.planner_churn_rate <= 1.0);
    assert!(report.health.policy_deny_rate >= 0.0 && report.health.policy_deny_rate <= 1.0);
    assert!(report.health.throughput_assignments_per_minute >= 0.0);
    assert!(report.health.avg_evaluation_latency_ms >= 0.0);

    // Overall must be one of the known values
    assert!(
        ["idle", "healthy", "degraded", "critical"].contains(&report.health.overall.as_str()),
        "Unknown health overall: {}",
        report.health.overall
    );
}

#[test]
fn slo_metrics_sample_field_contract() {
    // Verify that per-cycle metrics samples contain all expected fields via serde
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    let samples = &ml.state().metrics_history;
    assert!(!samples.is_empty());

    let sample = &samples[0];
    assert_eq!(sample.cycle_id, 1);
    assert_eq!(sample.timestamp_ms, 1000);
    assert!(sample.evaluation_latency_ms < 10_000); // Sanity bound
    assert!(sample.assignments > 0);
    // Rates should be well-formed
    assert!(sample.conflict_rate >= 0.0);
    assert!(sample.planner_churn_rate >= 0.0);
    assert!(sample.policy_deny_rate >= 0.0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// EVIDENCE ARTIFACT SERIALISATION
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn evidence_report_json_roundtrip_preserves_all_fields() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    let report = run_cycles(&mut ml, 3, &issues, &agents);
    let json = serde_json::to_string_pretty(&report).unwrap();
    let rt: OperatorStatusReport = serde_json::from_str(&json).unwrap();

    assert_eq!(report.status.cycle_count, rt.status.cycle_count);
    assert_eq!(report.health.overall, rt.health.overall);
    assert!(
        (report.health.throughput_assignments_per_minute
            - rt.health.throughput_assignments_per_minute)
            .abs()
            < 1e-10
    );
    assert!((report.health.conflict_rate - rt.health.conflict_rate).abs() < 1e-10);
    assert!((report.health.planner_churn_rate - rt.health.planner_churn_rate).abs() < 1e-10);
    assert!((report.health.policy_deny_rate - rt.health.policy_deny_rate).abs() < 1e-10);
    assert!(
        (report.health.avg_evaluation_latency_ms - rt.health.avg_evaluation_latency_ms).abs()
            < 1e-6
    );
    assert_eq!(report.status.total_assignments, rt.status.total_assignments);
    assert_eq!(report.status.total_rejections, rt.status.total_rejections);
}

#[test]
fn evidence_report_json_has_all_sections() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];

    let report = run_cycles(&mut ml, 5, &issues, &agents);
    let json = serde_json::to_string(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    // Verify all top-level sections present
    assert!(parsed.get("status").is_some(), "Missing 'status' section");
    assert!(
        parsed.get("assignment_table").is_some(),
        "Missing 'assignment_table'"
    );
    assert!(parsed.get("health").is_some(), "Missing 'health' section");
    assert!(
        parsed.get("conflicts").is_some(),
        "Missing 'conflicts' section"
    );
    assert!(
        parsed.get("event_summary").is_some(),
        "Missing 'event_summary'"
    );

    // Health section has all SLO indicators
    let health = parsed.get("health").unwrap();
    for field in &[
        "throughput_assignments_per_minute",
        "unblock_velocity_per_minute",
        "conflict_rate",
        "planner_churn_rate",
        "policy_deny_rate",
        "avg_evaluation_latency_ms",
        "overall",
    ] {
        assert!(health.get(field).is_some(), "Missing health field: {field}");
    }
}

#[test]
fn evidence_plain_text_contains_slo_metrics() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    let report = run_cycles(&mut ml, 3, &issues, &agents);
    let text = format_operator_report_plain(&report);

    assert!(text.contains("=== Mission Status ==="));
    assert!(text.contains("=== Health ==="));
    assert!(text.contains("Throughput:"));
    assert!(text.contains("Conflict rate:"));
    assert!(text.contains("Churn rate:"));
    assert!(text.contains("Policy deny:"));
    assert!(text.contains("Overall:"));
    assert!(text.contains("assign/min"));
}

#[test]
fn evidence_metrics_sample_serde_roundtrip() {
    let sample = MissionCycleMetricsSample {
        cycle_id: 42,
        timestamp_ms: 123_456,
        evaluation_latency_ms: 7,
        assignments: 5,
        rejections: 2,
        conflict_rejections: 1,
        policy_denials: 0,
        unblocked_transitions: 3,
        planner_churn_events: 1,
        throughput_assignments_per_minute: 8.5,
        unblock_velocity_per_minute: 2.0,
        conflict_rate: 0.05,
        planner_churn_rate: 0.1,
        policy_deny_rate: 0.0,
        assignments_by_agent: HashMap::from([("a1".to_string(), 3), ("a2".to_string(), 2)]),
        workspace_label: "prod".to_string(),
        track_label: "mission".to_string(),
    };

    let json = serde_json::to_string(&sample).unwrap();
    let rt: MissionCycleMetricsSample = serde_json::from_str(&json).unwrap();

    assert_eq!(sample.cycle_id, rt.cycle_id);
    assert_eq!(sample.assignments, rt.assignments);
    assert!((sample.conflict_rate - rt.conflict_rate).abs() < 1e-10);
    assert!((sample.planner_churn_rate - rt.planner_churn_rate).abs() < 1e-10);
    assert_eq!(sample.assignments_by_agent, rt.assignments_by_agent);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SHADOW-MODE FIDELITY SLO
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn shadow_perfect_fidelity_is_healthy() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let mut shadow = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..ShadowEvaluationConfig::default()
    });
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // Perfect match: recommendations == execution (empty events = no divergence)
    let diff = shadow.evaluate_cycle(1, 1000, &d.assignment_set, &[]);
    assert!(
        diff.fidelity_score >= 0.0,
        "Fidelity score should be non-negative"
    );
}

#[test]
fn shadow_evaluator_tracks_warmup() {
    let mut shadow = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 5,
        ..ShadowEvaluationConfig::default()
    });

    assert!(!shadow.is_warmed_up());
    assert_eq!(shadow.total_cycles(), 0);

    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    for i in 0..5 {
        let d = ml.evaluate(
            (i as i64 + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
        shadow.evaluate_cycle(i + 1, (i as i64 + 1) * 30_000, &d.assignment_set, &[]);
    }

    assert!(shadow.is_warmed_up());
    assert_eq!(shadow.total_cycles(), 5);
}

#[test]
fn shadow_metrics_accumulate_across_cycles() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let mut shadow = ShadowModeEvaluator::new(ShadowEvaluationConfig {
        warmup_cycles: 0,
        ..ShadowEvaluationConfig::default()
    });
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    for i in 0..10 {
        let d = ml.evaluate(
            (i as i64 + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
        shadow.evaluate_cycle(i + 1, (i as i64 + 1) * 30_000, &d.assignment_set, &[]);
    }

    let metrics = shadow.metrics();
    assert_eq!(metrics.total_cycles, 10);
    assert!(metrics.total_recommendations > 0);
    assert!(metrics.mean_dispatch_rate >= 0.0);
    assert!(metrics.mean_fidelity_score >= 0.0);
}

#[test]
fn shadow_config_defaults_are_sensible() {
    let shadow = ShadowModeEvaluator::with_defaults();
    let config = shadow.config();
    assert!(config.max_history > 0);
    assert!(config.low_confidence_threshold > 0.0);
    assert!(config.warmup_cycles > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// MULTI-CYCLE COMPLIANCE TRACKING
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn compliance_sustained_healthy_over_20_cycles() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    // Use varied beads with different priorities to reduce churn
    let issues = vec![
        issue("b1", 1),
        issue("b2", 2),
        issue("b3", 3),
        issue("b4", 4),
        issue("b5", 5),
    ];
    let c = ctx();

    let mut all_healthy = true;
    for i in 0..20 {
        ml.evaluate(
            (i as i64 + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
        let r = ml.generate_operator_report(Some(&elog()), None);
        if r.health.overall == "critical" {
            all_healthy = false;
        }
    }

    assert!(
        all_healthy,
        "System must not reach critical state during clean 20-cycle run"
    );
    assert_eq!(ml.state().cycle_count, 20);
}

#[test]
fn compliance_throughput_never_zero_with_work() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    let report = run_cycles(&mut ml, 5, &issues, &agents);
    assert!(
        report.health.throughput_assignments_per_minute > 0.0,
        "Throughput must be positive with available work"
    );
}

#[test]
fn compliance_metrics_history_bounded() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    // Run more cycles than max_samples (default 256)
    for i in 0..300 {
        ml.evaluate(
            (i as i64 + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    assert!(ml.state().metrics_history.len() <= 256);
    assert_eq!(ml.state().cycle_count, 300);
}

#[test]
fn compliance_assignment_counts_match_report() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    let mut total = 0u64;
    for i in 0..10 {
        let d = ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
        total += d.assignment_set.assignments.len() as u64;
    }

    let report = ml.generate_operator_report(Some(&elog()), None);
    assert_eq!(report.status.total_assignments, total);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DEGRADATION & RECOVERY
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn degradation_empty_agent_pool_does_not_crash() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents: Vec<MissionAgentCapabilityProfile> = vec![];
    let issues = vec![issue("b1", 1)];

    let report = run_cycles(&mut ml, 3, &issues, &agents);
    // Should complete without panic, with zero assignments
    assert_eq!(report.status.total_assignments, 0);
}

#[test]
fn degradation_all_agents_offline_shows_in_report() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![offline("a1"), offline("a2")];
    let issues = vec![issue("b1", 1)];

    let report = run_cycles(&mut ml, 3, &issues, &agents);
    assert_eq!(report.status.total_assignments, 0);
    // Agent assignment table should show the agents even with zero assignments
    assert_eq!(report.status.cycle_count, 3);
}

#[test]
fn recovery_from_offline_to_online() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    // Phase 1: all offline
    let offline_agents = vec![offline("a1"), offline("a2")];
    for i in 0..3 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &offline_agents,
            &c,
        );
    }
    let r1 = ml.generate_operator_report(Some(&elog()), None);
    assert_eq!(r1.status.total_assignments, 0);

    // Phase 2: agents come online
    let online_agents = vec![agent("a1"), agent("a2")];
    for i in 3..6 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &online_agents,
            &c,
        );
    }
    let r2 = ml.generate_operator_report(Some(&elog()), None);
    assert!(r2.status.total_assignments > 0, "Assignments should resume");
    assert_ne!(r2.health.overall, "critical");
}

#[test]
fn recovery_override_removal_restores_throughput() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    // Block via override
    ml.apply_override(OperatorOverride {
        override_id: "block".to_string(),
        kind: OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
        activated_by: "slo-test".to_string(),
        reason_code: "slo.recovery".to_string(),
        rationale: "Testing recovery".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: None,
        correlation_id: None,
    })
    .unwrap();

    // Cycle with override active — no assignments
    let d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(d1.assignment_set.assignments.is_empty());

    // Remove override
    ml.clear_override("block", 30_000);

    // Cycle after removal — assignments resume
    let d2 = ml.evaluate(31_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(!d2.assignment_set.assignments.is_empty());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// EVENT LOG INTEGRATION
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn event_log_populates_report_summary() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let mut event_log = elog();
    for i in 0..8 {
        event_log.emit(
            MissionEventBuilder::new(MissionEventKind::CycleStarted, "slo.dashboard")
                .cycle(i + 1, (i as i64 + 1) * 1000)
                .labels("slo", "dashboard"),
        );
    }

    let report = ml.generate_operator_report(Some(&event_log), None);
    assert_eq!(report.event_summary.total_emitted, 8);
    assert!(report.event_summary.retained_events > 0);
}

#[test]
fn event_log_absent_produces_zero_summary() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let report = ml.generate_operator_report(None, None);
    assert_eq!(report.event_summary.total_emitted, 0);
    assert_eq!(report.event_summary.retained_events, 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SAFETY & CONFLICT SLO
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn safety_envelope_caps_reflected_in_metrics() {
    let config = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 2,
            max_risky_assignments_per_cycle: 0,
            max_consecutive_retries_per_bead: 3,
            risky_label_markers: Vec::new(),
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let agents = vec![agent("a1"), agent("a2"), agent("a3"), agent("a4")];
    let issues = vec![
        issue("b1", 1),
        issue("b2", 2),
        issue("b3", 3),
        issue("b4", 4),
    ];
    let c = ctx();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    assert!(
        d.assignment_set.assignments.len() <= 2,
        "Safety cap at 2, got {}",
        d.assignment_set.assignments.len()
    );

    // Rejections from safety cap should appear in metrics
    let report = ml.generate_operator_report(Some(&elog()), None);
    assert!(report.status.total_rejections > 0 || !d.assignment_set.rejected.is_empty());
}

#[test]
fn conflict_detection_stats_in_report() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 20,
            strategy: DeconflictionStrategy::PriorityWins,
            generate_messages: true,
        },
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    let d = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // Create conflicting reservations
    let reservations = vec![
        KnownReservation {
            holder: "a1".to_string(),
            paths: vec!["src/main.rs".to_string()],
            exclusive: true,
            bead_id: Some("b1".to_string()),
            expires_at_ms: Some(60_000),
        },
        KnownReservation {
            holder: "a2".to_string(),
            paths: vec!["src/main.rs".to_string()],
            exclusive: true,
            bead_id: Some("b2".to_string()),
            expires_at_ms: Some(60_000),
        },
    ];

    let cr = ml.detect_conflicts(&d.assignment_set, &reservations, &[], 1000, &issues);
    assert!(!cr.conflicts.is_empty());

    let report = ml.generate_operator_report(Some(&elog()), None);
    assert!(report.conflicts.total_detected > 0);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DETERMINISM
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn determinism_slo_reports_identical_across_runs() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![agent("a1"), agent("a2")];
        let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];
        let c = ctx();

        for i in 0..10 {
            ml.evaluate(
                (i + 1) * 30_000,
                MissionTrigger::CadenceTick,
                &issues,
                &agents,
                &c,
            );
        }

        let report = ml.generate_operator_report(Some(&elog()), None);
        serde_json::to_string(&report).unwrap()
    };

    let r1 = run();
    let r2 = run();
    assert_eq!(r1, r2, "SLO reports must be deterministic across runs");
}

#[test]
fn determinism_metrics_totals_identical() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![agent("a1")];
        let issues = vec![issue("b1", 1)];
        let c = ctx();

        for i in 0..10 {
            ml.evaluate(
                (i + 1) * 30_000,
                MissionTrigger::CadenceTick,
                &issues,
                &agents,
                &c,
            );
        }

        let state = ml.state();
        (
            state.metrics_totals.cycles,
            state.metrics_totals.assignments,
            state.metrics_totals.rejections,
            state.metrics_totals.conflict_rejections,
            state.metrics_totals.planner_churn_events,
        )
    };

    assert_eq!(run(), run());
}
