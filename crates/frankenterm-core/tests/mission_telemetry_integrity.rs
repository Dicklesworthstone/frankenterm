//! Telemetry integrity tests and observability signal quality gates.
//! [ft-1i2ge.6.7]
//!
//! Validates that mission metrics and event streams are:
//! - Complete: all pipeline phases emit expected events
//! - Well-formed: events have correct phases, timestamps, reason codes
//! - Bounded: log memory does not grow unbounded
//! - Queryable: filtering by phase/cycle/kind works correctly
//! - Serde-stable: events and metrics roundtrip through JSON
//! - Deterministic: identical inputs produce identical telemetry

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{
    MissionEvent, MissionEventBuilder, MissionEventKind, MissionEventLog, MissionEventLogConfig,
    MissionPhase,
};
use frankenterm_core::mission_loop::{
    MissionLoop, MissionLoopConfig, MissionTrigger, format_operator_report_plain,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::PlannerExtractionContext;

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

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// EVENT TAXONOMY COMPLETENESS
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn taxonomy_all_event_kinds_have_valid_phase() {
    // Every MissionEventKind must map to a defined phase
    let all_kinds = vec![
        MissionEventKind::ReadinessResolved,
        MissionEventKind::FeaturesExtracted,
        MissionEventKind::ScoringCompleted,
        MissionEventKind::AssignmentsSolved,
        MissionEventKind::SafetyEnvelopeApplied,
        MissionEventKind::SafetyGateRejection,
        MissionEventKind::RetryStormThrottled,
        MissionEventKind::AssignmentEmitted,
        MissionEventKind::AssignmentRejected,
        MissionEventKind::ConflictDetected,
        MissionEventKind::ConflictAutoResolved,
        MissionEventKind::ConflictPendingManual,
        MissionEventKind::UnblockTransitionDetected,
        MissionEventKind::PlannerChurnDetected,
        MissionEventKind::CycleStarted,
        MissionEventKind::CycleCompleted,
        MissionEventKind::TriggerEnqueued,
        MissionEventKind::MetricsSampleRecorded,
    ];

    for kind in &all_kinds {
        let phase = kind.phase();
        // Phase must be one of the defined enum variants
        let is_valid = matches!(
            phase,
            MissionPhase::Plan
                | MissionPhase::Safety
                | MissionPhase::Dispatch
                | MissionPhase::Reconcile
                | MissionPhase::Lifecycle
        );
        assert!(
            is_valid,
            "Event kind {:?} has invalid phase {:?}",
            kind, phase
        );
    }
}

#[test]
fn taxonomy_event_kind_serde_roundtrip() {
    let kinds = vec![
        MissionEventKind::CycleStarted,
        MissionEventKind::ReadinessResolved,
        MissionEventKind::FeaturesExtracted,
        MissionEventKind::ScoringCompleted,
        MissionEventKind::AssignmentsSolved,
        MissionEventKind::SafetyEnvelopeApplied,
        MissionEventKind::AssignmentEmitted,
        MissionEventKind::ConflictDetected,
        MissionEventKind::CycleCompleted,
    ];

    for kind in kinds {
        let json = serde_json::to_string(&kind).unwrap();
        let rt: MissionEventKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, rt, "Roundtrip failed for {json}");
    }
}

#[test]
fn taxonomy_phase_serde_roundtrip() {
    for phase in [
        MissionPhase::Plan,
        MissionPhase::Safety,
        MissionPhase::Dispatch,
        MissionPhase::Reconcile,
        MissionPhase::Lifecycle,
    ] {
        let json = serde_json::to_string(&phase).unwrap();
        let rt: MissionPhase = serde_json::from_str(&json).unwrap();
        assert_eq!(phase, rt);
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// EVENT LOG INTEGRITY
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn log_emit_assigns_sequential_ids() {
    let mut log = elog();
    let id1 = log
        .emit(
            MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.seq")
                .cycle(1, 1000)
                .labels("test", "seq"),
        )
        .unwrap();
    let id2 = log
        .emit(
            MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.seq")
                .cycle(1, 1050)
                .labels("test", "seq"),
        )
        .unwrap();

    assert_eq!(id2, id1 + 1, "Event IDs must be sequential");
}

#[test]
fn log_disabled_returns_none() {
    let mut log = MissionEventLog::new(MissionEventLogConfig {
        max_events: 100,
        enabled: false,
    });

    let result = log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.disabled")
            .cycle(1, 1000)
            .labels("test", "disabled"),
    );
    assert!(result.is_none());
    assert!(log.events().is_empty());
}

#[test]
fn log_bounded_by_max_events() {
    let mut log = MissionEventLog::new(MissionEventLogConfig {
        max_events: 5,
        enabled: true,
    });

    for i in 0..10 {
        log.emit(
            MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.bounded")
                .cycle(i + 1, (i as i64 + 1) * 1000)
                .labels("test", "bounded"),
        );
    }

    assert_eq!(log.len(), 5);
    // Oldest events should be evicted (FIFO)
    let first_cycle = log.events()[0].cycle_id;
    assert!(first_cycle > 1, "Oldest events should be evicted");
}

#[test]
fn log_events_have_correct_timestamps() {
    let mut log = elog();

    log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.ts")
            .cycle(1, 5000)
            .labels("test", "ts"),
    );
    log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.ts")
            .cycle(1, 5050)
            .labels("test", "ts"),
    );

    let events = log.events();
    assert_eq!(events[0].timestamp_ms, 5000);
    assert_eq!(events[1].timestamp_ms, 5050);
    assert!(events[1].timestamp_ms >= events[0].timestamp_ms);
}

#[test]
fn log_event_serde_roundtrip() {
    let mut log = elog();

    log.emit(
        MissionEventBuilder::new(MissionEventKind::AssignmentEmitted, "test.serde")
            .cycle(42, 123_456)
            .correlation("corr-001")
            .labels("test", "serde")
            .detail_str("bead_id", "b1")
            .detail_str("agent_id", "a1"),
    );

    let event = &log.events()[0];
    let json = serde_json::to_string(event).unwrap();
    let rt: MissionEvent = serde_json::from_str(&json).unwrap();

    assert_eq!(event.kind, rt.kind);
    assert_eq!(event.cycle_id, rt.cycle_id);
    assert_eq!(event.timestamp_ms, rt.timestamp_ms);
    assert_eq!(event.reason_code, rt.reason_code);
    assert_eq!(event.correlation_id, rt.correlation_id);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// EVENT FILTERING / QUERY API
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn query_events_by_phase() {
    let mut log = elog();

    log.emit(
        MissionEventBuilder::new(MissionEventKind::ReadinessResolved, "test.phase")
            .cycle(1, 1000)
            .labels("test", "phase"),
    );
    log.emit(
        MissionEventBuilder::new(MissionEventKind::SafetyEnvelopeApplied, "test.phase")
            .cycle(1, 1010)
            .labels("test", "phase"),
    );
    log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.phase")
            .cycle(1, 1050)
            .labels("test", "phase"),
    );

    let plan_events = log.events_by_phase(MissionPhase::Plan);
    let safety_events = log.events_by_phase(MissionPhase::Safety);
    let lifecycle_events = log.events_by_phase(MissionPhase::Lifecycle);

    assert_eq!(plan_events.len(), 1);
    assert_eq!(safety_events.len(), 1);
    assert_eq!(lifecycle_events.len(), 1);
}

#[test]
fn query_events_by_cycle() {
    let mut log = elog();

    for cycle in 1..=3 {
        log.emit(
            MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.cycle")
                .cycle(cycle, cycle as i64 * 1000)
                .labels("test", "cycle"),
        );
        log.emit(
            MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.cycle")
                .cycle(cycle, cycle as i64 * 1000 + 50)
                .labels("test", "cycle"),
        );
    }

    let cycle_2 = log.events_by_cycle(2);
    assert_eq!(cycle_2.len(), 2);
    for e in &cycle_2 {
        assert_eq!(e.cycle_id, 2);
    }
}

#[test]
fn query_events_by_kind() {
    let mut log = elog();

    log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.kind")
            .cycle(1, 1000)
            .labels("test", "kind"),
    );
    log.emit(
        MissionEventBuilder::new(MissionEventKind::AssignmentEmitted, "test.kind")
            .cycle(1, 1010)
            .labels("test", "kind"),
    );
    log.emit(
        MissionEventBuilder::new(MissionEventKind::AssignmentEmitted, "test.kind")
            .cycle(1, 1020)
            .labels("test", "kind"),
    );

    let emitted = log.events_by_kind(&MissionEventKind::AssignmentEmitted);
    assert_eq!(emitted.len(), 2);
}

#[test]
fn query_drain_matching() {
    let mut log = elog();

    for i in 1..=5 {
        log.emit(
            MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.drain")
                .cycle(i, i as i64 * 1000)
                .labels("test", "drain"),
        );
    }

    assert_eq!(log.len(), 5);

    // Drain cycles > 3
    let drained = log.drain_matching(|e| e.cycle_id > 3);
    assert_eq!(drained.len(), 2);
    assert_eq!(log.len(), 3);
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// METRICS COMPLETENESS
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn metrics_sample_emitted_every_cycle() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    for i in 0..5 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    assert_eq!(ml.state().metrics_history.len(), 5);
    for (idx, sample) in ml.state().metrics_history.iter().enumerate() {
        assert_eq!(sample.cycle_id, idx as u64 + 1);
    }
}

#[test]
fn metrics_totals_consistent_with_samples() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
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
    let sum_assignments: usize = state.metrics_history.iter().map(|s| s.assignments).sum();
    let sum_rejections: usize = state.metrics_history.iter().map(|s| s.rejections).sum();

    assert_eq!(state.metrics_totals.assignments, sum_assignments as u64);
    assert_eq!(state.metrics_totals.rejections, sum_rejections as u64);
    assert_eq!(state.metrics_totals.cycles, 10);
}

#[test]
fn metrics_latency_recorded() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let latest = ml.latest_metrics().unwrap();
    // Latency should be >= 0 (nanosecond precision means it could be 0)
    assert!(latest.evaluation_latency_ms < 10_000, "Latency sanity check");
}

#[test]
fn metrics_workspace_and_track_labels_propagated() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let latest = ml.latest_metrics().unwrap();
    assert!(!latest.workspace_label.is_empty());
    assert!(!latest.track_label.is_empty());
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// REPORT SIGNAL QUALITY
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn report_event_summary_counts_by_phase() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let mut event_log = elog();
    // Emit events in different phases
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::ReadinessResolved, "test.summary")
            .cycle(1, 1000)
            .labels("test", "summary"),
    );
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::SafetyEnvelopeApplied, "test.summary")
            .cycle(1, 1010)
            .labels("test", "summary"),
    );
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::AssignmentEmitted, "test.summary")
            .cycle(1, 1020)
            .labels("test", "summary"),
    );
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.summary")
            .cycle(1, 1050)
            .labels("test", "summary"),
    );

    let report = ml.generate_operator_report(Some(&event_log), None);
    assert_eq!(report.event_summary.total_emitted, 4);
    assert!(report.event_summary.retained_events >= 4);

    // by_phase should have entries for the phases we used
    assert!(!report.event_summary.by_phase.is_empty());
}

#[test]
fn report_event_summary_counts_by_kind() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let mut event_log = elog();
    for _ in 0..3 {
        event_log.emit(
            MissionEventBuilder::new(MissionEventKind::AssignmentEmitted, "test.by_kind")
                .cycle(1, 1000)
                .labels("test", "by_kind"),
        );
    }
    event_log.emit(
        MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.by_kind")
            .cycle(1, 1050)
            .labels("test", "by_kind"),
    );

    let report = ml.generate_operator_report(Some(&event_log), None);
    assert!(!report.event_summary.by_kind.is_empty());
}

#[test]
fn report_json_stable_schema() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];

    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx());

    let report = ml.generate_operator_report(Some(&elog()), None);
    let json = serde_json::to_string_pretty(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    // All top-level sections must be present
    for section in &[
        "status",
        "assignment_table",
        "health",
        "conflicts",
        "event_summary",
    ] {
        assert!(
            parsed.get(section).is_some(),
            "Missing report section: {section}"
        );
    }

    // Health indicators must be present
    let health = parsed.get("health").unwrap();
    for field in &[
        "overall",
        "throughput_assignments_per_minute",
        "conflict_rate",
        "planner_churn_rate",
        "policy_deny_rate",
    ] {
        assert!(
            health.get(field).is_some(),
            "Missing health field: {field}"
        );
    }
}

#[test]
fn report_plain_text_contains_all_sections() {
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

    let report = ml.generate_operator_report(Some(&elog()), None);
    let text = format_operator_report_plain(&report);

    // Standard operator-facing sections
    assert!(text.contains("=== Mission Status ==="));
    assert!(text.contains("=== Health ==="));
    assert!(text.contains("Phase:"));
    assert!(text.contains("Cycles:"));
    assert!(text.contains("Overall:"));
    assert!(text.contains("Throughput:"));
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// SIGNAL TRUSTWORTHINESS
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn trust_rates_bounded_zero_to_one() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1")];
    let issues = vec![issue("b1", 1)];
    let c = ctx();

    for i in 0..5 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    for sample in &ml.state().metrics_history {
        assert!(
            sample.conflict_rate >= 0.0 && sample.conflict_rate <= 1.0,
            "conflict_rate out of range: {}",
            sample.conflict_rate
        );
        assert!(
            sample.planner_churn_rate >= 0.0 && sample.planner_churn_rate <= 1.0,
            "planner_churn_rate out of range: {}",
            sample.planner_churn_rate
        );
        assert!(
            sample.policy_deny_rate >= 0.0 && sample.policy_deny_rate <= 1.0,
            "policy_deny_rate out of range: {}",
            sample.policy_deny_rate
        );
    }
}

#[test]
fn trust_cycle_ids_monotonic() {
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

    let history = &ml.state().metrics_history;
    for window in history.windows(2) {
        assert!(
            window[1].cycle_id > window[0].cycle_id,
            "Cycle IDs must be monotonically increasing"
        );
    }
}

#[test]
fn trust_timestamps_non_decreasing() {
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

    let history = &ml.state().metrics_history;
    for window in history.windows(2) {
        assert!(
            window[1].timestamp_ms >= window[0].timestamp_ms,
            "Timestamps must be non-decreasing"
        );
    }
}

#[test]
fn trust_assignment_count_matches_agent_breakdown() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = vec![agent("a1"), agent("a2")];
    let issues = vec![issue("b1", 1), issue("b2", 2)];
    let c = ctx();

    for i in 0..5 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    for sample in &ml.state().metrics_history {
        let agent_sum: u64 = sample.assignments_by_agent.values().sum();
        assert_eq!(
            sample.assignments as u64, agent_sum,
            "Cycle {} assignment count mismatch",
            sample.cycle_id
        );
    }
}

// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
// DETERMINISM
// ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

#[test]
fn determinism_event_log_identical_across_runs() {
    let run = || {
        let mut log = elog();
        for i in 1..=10 {
            log.emit(
                MissionEventBuilder::new(MissionEventKind::CycleStarted, "test.det")
                    .cycle(i, i as i64 * 1000)
                    .labels("test", "determinism"),
            );
            log.emit(
                MissionEventBuilder::new(MissionEventKind::CycleCompleted, "test.det")
                    .cycle(i, i as i64 * 1000 + 50)
                    .labels("test", "determinism"),
            );
        }
        let events: Vec<String> = log
            .events()
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        events
    };

    assert_eq!(run(), run());
}

#[test]
fn determinism_metrics_identical_across_runs() {
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

        ml.state()
            .metrics_history
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect::<Vec<serde_json::Value>>()
    };

    assert_eq!(run(), run());
}

#[test]
fn determinism_report_json_identical() {
    let run = || {
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let agents = vec![agent("a1")];
        let issues = vec![issue("b1", 1)];
        let c = ctx();

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
        serde_json::to_value(&report).unwrap()
    };

    assert_eq!(run(), run());
}
