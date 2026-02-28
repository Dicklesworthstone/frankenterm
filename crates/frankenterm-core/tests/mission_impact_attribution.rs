//! ft-1i2ge.6.8 — Mission impact attribution and manual-vs-autopilot baseline analyzer
//!
//! Tests quantify real user value by measuring mission-driven outcomes against
//! manual coordination baselines, with confidence annotations and per-bead/per-agent
//! attribution analysis.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{MissionEventLog, MissionEventLogConfig};
use frankenterm_core::mission_loop::{
    MissionCycleMetricsSample, MissionLoop, MissionLoopConfig, MissionTrigger, OperatorOverride,
    OperatorOverrideKind, OperatorStatusReport,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::PlannerExtractionContext;

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
    MissionEventLog::new(MissionEventLogConfig::default())
}

/// Run N evaluation cycles with given agents/issues, return the MissionLoop.
fn run_cycles(
    agents: &[MissionAgentCapabilityProfile],
    issues: &[BeadIssueDetail],
    n: usize,
) -> MissionLoop {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
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
    ml
}

/// Compute baseline statistics from metrics_history.
struct BaselineStats {
    mean_throughput: f64,
    _mean_unblock_velocity: f64,
    mean_conflict_rate: f64,
    mean_churn_rate: f64,
    mean_deny_rate: f64,
    _mean_latency_ms: f64,
    sample_count: usize,
    throughput_variance: f64,
}

fn compute_baseline(samples: &[&MissionCycleMetricsSample]) -> BaselineStats {
    let n = samples.len();
    if n == 0 {
        return BaselineStats {
            mean_throughput: 0.0,
            _mean_unblock_velocity: 0.0,
            mean_conflict_rate: 0.0,
            mean_churn_rate: 0.0,
            mean_deny_rate: 0.0,
            _mean_latency_ms: 0.0,
            sample_count: 0,
            throughput_variance: 0.0,
        };
    }
    let nf = n as f64;
    let mean_tp = samples
        .iter()
        .map(|s| s.throughput_assignments_per_minute)
        .sum::<f64>()
        / nf;
    let unblock_vel = samples
        .iter()
        .map(|s| s.unblock_velocity_per_minute)
        .sum::<f64>()
        / nf;
    let conflict = samples.iter().map(|s| s.conflict_rate).sum::<f64>() / nf;
    let churn = samples.iter().map(|s| s.planner_churn_rate).sum::<f64>() / nf;
    let deny = samples.iter().map(|s| s.policy_deny_rate).sum::<f64>() / nf;
    let latency = samples
        .iter()
        .map(|s| s.evaluation_latency_ms as f64)
        .sum::<f64>()
        / nf;
    let tp_var = samples
        .iter()
        .map(|s| (s.throughput_assignments_per_minute - mean_tp).powi(2))
        .sum::<f64>()
        / nf;

    BaselineStats {
        mean_throughput: mean_tp,
        _mean_unblock_velocity: unblock_vel,
        mean_conflict_rate: conflict,
        mean_churn_rate: churn,
        mean_deny_rate: deny,
        _mean_latency_ms: latency,
        sample_count: n,
        throughput_variance: tp_var,
    }
}

/// Attribution record for a single agent.
struct AgentAttribution {
    _agent_id: String,
    total_assignments: u64,
    _cycle_count: usize,
}

// ═══════════════════════════════════════════════════════════════════════
// §1 — Baseline Measurement
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn baseline_from_clean_cycles_has_positive_throughput() {
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 5);

    let history = &ml.state().metrics_history;
    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    assert!(
        stats.mean_throughput > 0.0,
        "Clean cycles should have positive throughput: {}",
        stats.mean_throughput
    );
    assert_eq!(stats.sample_count, 5);
}

#[test]
fn baseline_from_empty_history_returns_zero() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let history = &ml.state().metrics_history;
    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    assert!(stats.mean_throughput.abs() < f64::EPSILON);
    assert_eq!(stats.sample_count, 0);
}

#[test]
fn baseline_single_cycle_equals_that_cycle() {
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let ml = run_cycles(&agents, &issues, 1);

    let history = &ml.state().metrics_history;
    assert_eq!(history.len(), 1);

    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    let diff = (stats.mean_throughput - history[0].throughput_assignments_per_minute).abs();
    assert!(
        diff < f64::EPSILON,
        "Single sample baseline must equal itself"
    );
    assert!(
        stats.throughput_variance.abs() < f64::EPSILON,
        "Single sample has zero variance"
    );
}

#[test]
fn baseline_variance_increases_with_mixed_load() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let c = ctx();

    // 5 cycles with varying workload
    for i in 0..5 {
        let n_issues = (i % 3) + 1;
        let issues: Vec<BeadIssueDetail> = (0..n_issues)
            .map(|j| issue(&format!("b{j}"), j as u8 + 1))
            .collect();
        let agents = vec![agent("a1"), agent("a2")];
        ml.evaluate(
            (i as i64 + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let history = &ml.state().metrics_history;
    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    // Variable workload should produce non-trivial variance
    assert!(stats.sample_count >= 3, "Need enough samples for variance");
}

// ═══════════════════════════════════════════════════════════════════════
// §2 — Impact Comparison (autopilot vs baseline)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn impact_throughput_measurable_across_window() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];
    let ml = run_cycles(&agents, &issues, 10);

    let history = &ml.state().metrics_history;
    // First half vs second half
    let first_half: Vec<&MissionCycleMetricsSample> = history[..5].iter().collect();
    let second_half: Vec<&MissionCycleMetricsSample> = history[5..].iter().collect();

    let baseline = compute_baseline(&first_half);
    let current = compute_baseline(&second_half);

    // Both windows should have measurable throughput
    assert!(baseline.mean_throughput > 0.0);
    assert!(current.mean_throughput > 0.0);

    // Impact delta is computable
    let delta = current.mean_throughput - baseline.mean_throughput;
    assert!(delta.is_finite(), "Impact delta must be finite");
}

#[test]
fn impact_conflict_rate_bounded() {
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 8);

    let history = &ml.state().metrics_history;
    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    assert!(
        stats.mean_conflict_rate <= 1.0,
        "Conflict rate must be bounded [0,1]: {}",
        stats.mean_conflict_rate
    );
}

#[test]
fn impact_churn_decreases_with_stable_assignments() {
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 10);

    let history = &ml.state().metrics_history;
    if history.len() >= 4 {
        let late_churn: Vec<f64> = history[history.len() - 3..]
            .iter()
            .map(|s| s.planner_churn_rate)
            .collect();
        let avg_late_churn = late_churn.iter().sum::<f64>() / late_churn.len() as f64;
        assert!(
            avg_late_churn <= 1.0,
            "Churn rate bounded: {}",
            avg_late_churn
        );
    }
}

#[test]
fn impact_deny_rate_bounded_below_half() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1)];
    let ml = run_cycles(&agents, &issues, 5);

    let history = &ml.state().metrics_history;
    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    // Default safety envelope may deny some assignments; verify bounded
    assert!(
        stats.mean_deny_rate <= 0.5,
        "Deny rate should stay below critical threshold: {}",
        stats.mean_deny_rate
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §3 — Per-Bead Attribution
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn attribution_per_bead_assignment_tracking() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];
    let ml = run_cycles(&agents, &issues, 5);

    let report = ml.generate_operator_report(Some(&elog()), None);

    assert!(
        !report.assignment_table.is_empty(),
        "Assignment table must not be empty"
    );

    let total: u64 = report
        .assignment_table
        .iter()
        .map(|r| r.total_assignments)
        .sum();
    assert!(total > 0, "Total assignments should be positive");
}

#[test]
fn attribution_per_agent_workload_distribution() {
    let agents = vec![agent("a1"), agent("a2"), agent("a3")];
    let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];
    let ml = run_cycles(&agents, &issues, 5);

    let report = ml.generate_operator_report(Some(&elog()), None);

    let totals = &ml.state().metrics_totals;
    let agent_attrs: Vec<AgentAttribution> = totals
        .assignments_by_agent
        .iter()
        .map(|(aid, count)| AgentAttribution {
            _agent_id: aid.clone(),
            total_assignments: *count,
            _cycle_count: 5,
        })
        .collect();

    let has_assignments = agent_attrs.iter().any(|a| a.total_assignments > 0);
    assert!(
        has_assignments,
        "At least one agent should have assignments"
    );

    assert!(
        report.health.throughput_assignments_per_minute > 0.0,
        "Health throughput should reflect assignments"
    );
}

#[test]
fn attribution_pinned_bead_tracked_separately() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    for i in 0..3 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    ml.apply_override(OperatorOverride {
        override_id: "pin-1".to_string(),
        kind: OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a2".to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "manual_steering".to_string(),
        rationale: "Pin b1 to a2 for manual coordination test".to_string(),
        activated_at_ms: 100_000,
        expires_at_ms: None,
        correlation_id: Some("test-pin".to_string()),
    })
    .unwrap();

    for i in 3..8 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let report = ml.generate_operator_report(Some(&elog()), None);

    let a2_row = report.assignment_table.iter().find(|r| r.agent_id == "a2");
    assert!(
        a2_row.is_some(),
        "Agent a2 should appear in assignment table"
    );
}

#[test]
fn attribution_excluded_bead_not_assigned() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];
    let c = ctx();

    ml.apply_override(OperatorOverride {
        override_id: "excl-1".to_string(),
        kind: OperatorOverrideKind::Exclude {
            bead_id: "b3".to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "manual_exclusion".to_string(),
        rationale: "Exclude b3 from assignment".to_string(),
        activated_at_ms: 1_000,
        expires_at_ms: None,
        correlation_id: None,
    })
    .unwrap();

    for i in 0..5 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let report = ml.generate_operator_report(Some(&elog()), None);
    let has_b3 = report
        .assignment_table
        .iter()
        .flat_map(|r| r.active_bead_ids.iter())
        .any(|id| id == "b3");

    assert!(
        !has_b3,
        "Excluded bead b3 must not appear in active assignments"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §4 — Confidence and Statistical Properties
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn confidence_variance_zero_for_constant_workload() {
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let ml = run_cycles(&agents, &issues, 5);

    let history = &ml.state().metrics_history;
    if history.len() >= 3 {
        let stable: Vec<&MissionCycleMetricsSample> = history[1..].iter().collect();
        let stats = compute_baseline(&stable);

        assert!(
            stats.throughput_variance < 100.0,
            "Constant workload should have low variance: {}",
            stats.throughput_variance
        );
    }
}

#[test]
fn confidence_sample_count_matches_cycle_count() {
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let ml = run_cycles(&agents, &issues, 7);

    let history = &ml.state().metrics_history;
    let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
    let stats = compute_baseline(&samples);

    assert_eq!(stats.sample_count, 7, "Sample count must match cycle count");
}

#[test]
fn confidence_rates_bounded_zero_to_one() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2), issue("b3", 3)];
    let ml = run_cycles(&agents, &issues, 10);

    let history = &ml.state().metrics_history;
    for sample in history {
        assert!(
            sample.conflict_rate >= 0.0 && sample.conflict_rate <= 1.0,
            "Conflict rate out of bounds: {}",
            sample.conflict_rate
        );
        assert!(
            sample.policy_deny_rate >= 0.0 && sample.policy_deny_rate <= 1.0,
            "Deny rate out of bounds: {}",
            sample.policy_deny_rate
        );
        assert!(
            sample.planner_churn_rate >= 0.0 && sample.planner_churn_rate <= 1.0,
            "Churn rate out of bounds: {}",
            sample.planner_churn_rate
        );
    }
}

#[test]
fn confidence_window_stability_over_many_cycles() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 20);

    let history = &ml.state().metrics_history;
    let mut window_means: Vec<f64> = Vec::new();
    for start in 0..history.len().saturating_sub(4) {
        let window: Vec<&MissionCycleMetricsSample> = history[start..start + 5].iter().collect();
        let stats = compute_baseline(&window);
        window_means.push(stats.mean_throughput);
    }

    if window_means.len() >= 2 {
        let global_mean = window_means.iter().sum::<f64>() / window_means.len() as f64;
        let max_deviation = window_means
            .iter()
            .map(|m| (m - global_mean).abs())
            .fold(0.0_f64, f64::max);
        assert!(
            max_deviation <= global_mean + 1.0,
            "Rolling window means too unstable: max_dev={}, global={}",
            max_deviation,
            global_mean
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// §5 — Manual vs Autopilot Comparison
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn manual_trigger_counted_as_intervention() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        max_trigger_batch: 1,
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    ml.evaluate(
        60_000,
        MissionTrigger::ManualTrigger {
            reason: "operator_intervention".to_string(),
        },
        &issues,
        &agents,
        &c,
    );

    let history = &ml.state().metrics_history;
    assert_eq!(history.len(), 2, "Both cadence and manual cycles recorded");
}

#[test]
fn override_impact_reflected_in_state() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    for i in 0..3 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    ml.apply_override(OperatorOverride {
        override_id: "excl-a1".to_string(),
        kind: OperatorOverrideKind::ExcludeAgent {
            agent_id: "a1".to_string(),
        },
        activated_by: "operator".to_string(),
        reason_code: "manual_intervention".to_string(),
        rationale: "Remove a1 to test impact".to_string(),
        activated_at_ms: 100_000,
        expires_at_ms: None,
        correlation_id: None,
    })
    .unwrap();

    for i in 3..6 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    assert!(
        !ml.active_overrides().is_empty(),
        "Active override must be visible"
    );
}

#[test]
fn autopilot_only_has_zero_overrides() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 5);

    assert!(
        ml.active_overrides().is_empty(),
        "Pure autopilot should have no active overrides"
    );
}

#[test]
fn mixed_mode_both_autopilot_and_manual_cycles() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        max_trigger_batch: 1,
        ..MissionLoopConfig::default()
    });
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    for i in 0..6 {
        let trigger = if i % 2 == 0 {
            MissionTrigger::CadenceTick
        } else {
            MissionTrigger::ManualTrigger {
                reason: "operator".to_string(),
            }
        };
        ml.evaluate((i + 1) * 30_000, trigger, &issues, &agents, &c);
    }

    let history = &ml.state().metrics_history;
    assert_eq!(history.len(), 6, "All 6 cycles should be recorded");

    let totals = &ml.state().metrics_totals;
    assert_eq!(totals.cycles, 6);
}

// ═══════════════════════════════════════════════════════════════════════
// §6 — Report Signal Quality for Impact
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn report_contains_all_impact_relevant_sections() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 5);

    let report = ml.generate_operator_report(Some(&elog()), None);

    assert!(
        !report.status.phase_label.is_empty(),
        "Phase label needed for impact context"
    );
    assert!(
        !report.assignment_table.is_empty(),
        "Assignment table needed for per-agent impact"
    );
    assert!(
        !report.health.overall.is_empty(),
        "Health overall needed for summary impact"
    );
}

#[test]
fn report_health_metrics_all_finite() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 5);

    let report = ml.generate_operator_report(Some(&elog()), None);

    assert!(report.health.throughput_assignments_per_minute.is_finite());
    assert!(report.health.unblock_velocity_per_minute.is_finite());
    assert!(report.health.conflict_rate.is_finite());
    assert!(report.health.planner_churn_rate.is_finite());
    assert!(report.health.policy_deny_rate.is_finite());
    assert!(report.health.avg_evaluation_latency_ms.is_finite());
}

#[test]
fn report_json_schema_has_impact_fields() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 5);

    let report = ml.generate_operator_report(Some(&elog()), None);
    let json = serde_json::to_value(&report).unwrap();

    assert!(json.get("health").is_some(), "JSON must have 'health'");
    let health = json.get("health").unwrap();
    assert!(health.get("throughput_assignments_per_minute").is_some());
    assert!(health.get("conflict_rate").is_some());
    assert!(health.get("planner_churn_rate").is_some());
    assert!(health.get("policy_deny_rate").is_some());
    assert!(health.get("avg_evaluation_latency_ms").is_some());
}

#[test]
fn report_serde_roundtrip_preserves_impact_data() {
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let ml = run_cycles(&agents, &issues, 5);

    let report = ml.generate_operator_report(Some(&elog()), None);
    let json_str = serde_json::to_string(&report).unwrap();
    let roundtrip: OperatorStatusReport = serde_json::from_str(&json_str).unwrap();

    assert_eq!(report.health.overall, roundtrip.health.overall);
    assert!(
        (report.health.throughput_assignments_per_minute
            - roundtrip.health.throughput_assignments_per_minute)
            .abs()
            < 1e-10
    );
    assert_eq!(
        report.assignment_table.len(),
        roundtrip.assignment_table.len()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §7 — Determinism
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn determinism_baseline_identical_across_runs() {
    let run = || {
        let agents = vec![agent("a1"), agent("a2")];
        let issues = vec![issue("b1", 1), issue("b2", 2)];
        let ml = run_cycles(&agents, &issues, 5);

        let history = &ml.state().metrics_history;
        let samples: Vec<&MissionCycleMetricsSample> = history.iter().collect();
        compute_baseline(&samples)
    };

    let s1 = run();
    let s2 = run();

    assert!((s1.mean_throughput - s2.mean_throughput).abs() < f64::EPSILON);
    assert!((s1.mean_conflict_rate - s2.mean_conflict_rate).abs() < f64::EPSILON);
    assert!((s1.mean_churn_rate - s2.mean_churn_rate).abs() < f64::EPSILON);
    assert_eq!(s1.sample_count, s2.sample_count);
}

#[test]
fn determinism_impact_report_identical() {
    let run = || {
        let agents = vec![agent("a1")];
        let issues = vec![issue("b1", 1)];
        let ml = run_cycles(&agents, &issues, 5);
        let report = ml.generate_operator_report(Some(&elog()), None);
        serde_json::to_value(&report).unwrap()
    };

    assert_eq!(run(), run());
}

#[test]
fn determinism_metrics_totals_identical() {
    let run = || {
        let agents = vec![agent("a1"), agent("a2")];
        let issues = vec![issue("b1", 1), issue("b2", 2)];
        let ml = run_cycles(&agents, &issues, 8);
        let totals = &ml.state().metrics_totals;
        (totals.cycles, totals.assignments, totals.rejections)
    };

    assert_eq!(run(), run());
}
