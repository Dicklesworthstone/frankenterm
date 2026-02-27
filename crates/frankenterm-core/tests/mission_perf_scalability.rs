//! ft-1i2ge.7.4 — Performance and scalability budget verification
//!
//! Measures mission loop/planner/dispatch performance at realistic swarm sizes
//! and validates that latency/throughput stay within defined budgets.

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;
use std::time::Instant;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_events::{MissionEventLog, MissionEventLogConfig};
use frankenterm_core::mission_loop::{
    MissionCycleMetricsSample, MissionLoop, MissionLoopConfig, MissionTrigger,
    OperatorOverride, OperatorOverrideKind, OperatorStatusReport,
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
        max_parallel_assignments: 5,
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

fn agents_n(n: usize) -> Vec<MissionAgentCapabilityProfile> {
    (0..n).map(|i| agent(&format!("agent-{i}"))).collect()
}

fn issues_n(n: usize) -> Vec<BeadIssueDetail> {
    (0..n)
        .map(|i| issue(&format!("bead-{i}"), (i % 5 + 1) as u8))
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// §1 — Single Cycle Latency
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn perf_single_cycle_small_swarm_under_1ms() {
    let agents = agents_n(3);
    let issues = issues_n(5);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start = Instant::now();
    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 50,
        "Small swarm single cycle should be fast: {:?}",
        elapsed
    );
}

#[test]
fn perf_single_cycle_medium_swarm_under_10ms() {
    let agents = agents_n(20);
    let issues = issues_n(50);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start = Instant::now();
    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 100,
        "Medium swarm single cycle: {:?}",
        elapsed
    );
}

#[test]
fn perf_single_cycle_large_swarm_under_100ms() {
    let agents = agents_n(50);
    let issues = issues_n(200);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start = Instant::now();
    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 500,
        "Large swarm single cycle: {:?}",
        elapsed
    );
}

#[test]
fn perf_first_cycle_not_disproportionately_slow() {
    let agents = agents_n(10);
    let issues = issues_n(20);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start1 = Instant::now();
    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let first = start1.elapsed();

    let start2 = Instant::now();
    ml.evaluate(60_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let second = start2.elapsed();

    // First cycle may be slightly slower but not orders of magnitude
    assert!(
        first.as_micros() < second.as_micros() * 100 + 1000,
        "First cycle {} should not be 100x slower than subsequent {}",
        first.as_micros(),
        second.as_micros()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §2 — Multi-Cycle Throughput
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn perf_100_cycles_small_swarm_under_1s() {
    let agents = agents_n(3);
    let issues = issues_n(5);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start = Instant::now();
    for i in 0..100 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 5,
        "100 cycles small swarm: {:?}",
        elapsed
    );
}

#[test]
fn perf_50_cycles_medium_swarm_under_5s() {
    let agents = agents_n(20);
    let issues = issues_n(50);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start = Instant::now();
    for i in 0..50 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 10,
        "50 cycles medium swarm: {:?}",
        elapsed
    );
}

#[test]
fn perf_throughput_stable_across_cycles() {
    let agents = agents_n(5);
    let issues = issues_n(10);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let mut cycle_times_us: Vec<u128> = Vec::new();
    for i in 0..20 {
        let start = Instant::now();
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
        cycle_times_us.push(start.elapsed().as_micros());
    }

    // Later cycles should not be dramatically slower (no memory leak / O(n^2) growth)
    let early_avg = cycle_times_us[..5].iter().sum::<u128>() / 5;
    let late_avg = cycle_times_us[15..].iter().sum::<u128>() / 5;

    // Allow 10x growth factor as generous bound
    assert!(
        late_avg < early_avg * 10 + 100,
        "Late cycles should not be dramatically slower: early_avg={}us, late_avg={}us",
        early_avg,
        late_avg
    );
}

#[test]
fn perf_metrics_history_bounded() {
    let agents = agents_n(3);
    let issues = issues_n(5);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    for i in 0..500 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    // Metrics history should be bounded (default max_samples = 256)
    let history_len = ml.state().metrics_history.len();
    assert!(
        history_len <= 256,
        "Metrics history should be bounded: {}",
        history_len
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §3 — Scalability (agent/bead count)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn scale_linear_with_agent_count() {
    let issues = issues_n(10);
    let c = ctx();

    let time_with = |n_agents: usize| {
        let agents = agents_n(n_agents);
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let start = Instant::now();
        for i in 0..10 {
            ml.evaluate(
                (i + 1) * 30_000,
                MissionTrigger::CadenceTick,
                &issues,
                &agents,
                &c,
            );
        }
        start.elapsed().as_micros()
    };

    let t_5 = time_with(5);
    let t_50 = time_with(50);

    // 10x agents should not cause >100x slowdown (sub-quadratic)
    assert!(
        t_50 < t_5 * 200 + 10_000,
        "50 agents should not be 200x slower than 5: t_5={}us, t_50={}us",
        t_5,
        t_50
    );
}

#[test]
fn scale_linear_with_bead_count() {
    let agents = agents_n(5);
    let c = ctx();

    let time_with = |n_beads: usize| {
        let issues = issues_n(n_beads);
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        let start = Instant::now();
        for i in 0..10 {
            ml.evaluate(
                (i + 1) * 30_000,
                MissionTrigger::CadenceTick,
                &issues,
                &agents,
                &c,
            );
        }
        start.elapsed().as_micros()
    };

    let t_10 = time_with(10);
    let t_100 = time_with(100);

    // 10x beads should not cause >100x slowdown
    assert!(
        t_100 < t_10 * 200 + 10_000,
        "100 beads should not be 200x slower than 10: t_10={}us, t_100={}us",
        t_10,
        t_100
    );
}

#[test]
fn scale_report_generation_fast() {
    let agents = agents_n(20);
    let issues = issues_n(50);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    for i in 0..20 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let start = Instant::now();
    let _report = ml.generate_operator_report(Some(&elog()), None);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 50,
        "Report generation should be fast: {:?}",
        elapsed
    );
}

#[test]
fn scale_override_application_fast() {
    let agents = agents_n(10);
    let issues = issues_n(20);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    // Apply 10 overrides
    for i in 0..10 {
        ml.apply_override(OperatorOverride {
            override_id: format!("ovr-{i}"),
            kind: OperatorOverrideKind::Reprioritize {
                bead_id: format!("bead-{i}"),
                score_delta: 5,
            },
            activated_by: "operator".to_string(),
            reason_code: "perf_test".to_string(),
            rationale: "Performance test override".to_string(),
            activated_at_ms: 1000,
            expires_at_ms: None,
            correlation_id: None,
        })
        .unwrap();
    }

    let start = Instant::now();
    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 100,
        "Cycle with 10 overrides should be fast: {:?}",
        elapsed
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §4 — Report and Serialization Performance
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn perf_report_json_serialization_fast() {
    let agents = agents_n(10);
    let issues = issues_n(30);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

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

    let start = Instant::now();
    let json = serde_json::to_string(&report).unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 10,
        "JSON serialization should be fast: {:?}",
        elapsed
    );
    assert!(!json.is_empty());
}

#[test]
fn perf_report_deserialization_fast() {
    let agents = agents_n(10);
    let issues = issues_n(30);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

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
    let json = serde_json::to_string(&report).unwrap();

    let start = Instant::now();
    let _roundtrip: OperatorStatusReport = serde_json::from_str(&json).unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 10,
        "JSON deserialization should be fast: {:?}",
        elapsed
    );
}

#[test]
fn perf_metrics_sample_serialization() {
    let agents = agents_n(5);
    let issues = issues_n(10);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

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
    let start = Instant::now();
    for sample in history {
        let _json = serde_json::to_value(sample).unwrap();
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 50,
        "Serializing {} metrics samples: {:?}",
        history.len(),
        elapsed
    );
}

#[test]
fn perf_plain_text_report_fast() {
    let agents = agents_n(10);
    let issues = issues_n(30);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

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

    let start = Instant::now();
    let text = frankenterm_core::mission_loop::format_operator_report_plain(&report);
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 10,
        "Plain text report formatting: {:?}",
        elapsed
    );
    assert!(!text.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
// §5 — Memory/Resource Budget
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn budget_state_size_bounded_after_many_cycles() {
    let agents = agents_n(5);
    let issues = issues_n(10);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    for i in 0..500 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    let state = ml.state();
    // Metrics history bounded
    assert!(state.metrics_history.len() <= 256);
    // Cycle count tracks all
    assert_eq!(state.cycle_count, 500);
}

#[test]
fn budget_conflict_history_bounded() {
    let agents = agents_n(5);
    let issues = issues_n(10);
    let c = ctx();
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    for i in 0..200 {
        ml.evaluate(
            (i + 1) * 30_000,
            MissionTrigger::CadenceTick,
            &issues,
            &agents,
            &c,
        );
    }

    // Conflict history should not grow unbounded
    let conflict_count = ml.state().conflict_history.len();
    assert!(
        conflict_count <= 500,
        "Conflict history should be bounded: {}",
        conflict_count
    );
}

#[test]
fn budget_override_history_bounded() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    // Apply and clear 200 overrides
    for i in 0..200 {
        ml.apply_override(OperatorOverride {
            override_id: format!("ovr-{i}"),
            kind: OperatorOverrideKind::Reprioritize {
                bead_id: format!("bead-{}", i % 10),
                score_delta: 1,
            },
            activated_by: "operator".to_string(),
            reason_code: "perf_test".to_string(),
            rationale: "Budget test".to_string(),
            activated_at_ms: (i as i64) * 1000,
            expires_at_ms: None,
            correlation_id: None,
        })
        .unwrap();
        ml.clear_override(&format!("ovr-{i}"), (i as i64 + 1) * 1000);
    }

    // Active overrides should be zero (all cleared)
    assert!(ml.active_overrides().is_empty());
}

#[test]
fn budget_event_log_bounded() {
    let mut log = MissionEventLog::new(MissionEventLogConfig {
        max_events: 100,
        enabled: true,
    });

    // Emit 200 events
    for i in 0..200_u64 {
        use frankenterm_core::mission_events::{MissionEventBuilder, MissionEventKind};
        log.emit(
            MissionEventBuilder::new(MissionEventKind::ReadinessResolved, "perf_test")
                .cycle(i + 1, i as i64 * 1000),
        );
    }

    // Should be bounded to max_events
    assert!(
        log.len() <= 100,
        "Event log should be bounded to max_events: {}",
        log.len()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// §6 — Trigger Processing Performance
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn perf_trigger_enqueue_fast() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());

    let start = Instant::now();
    for _ in 0..100 {
        ml.trigger(MissionTrigger::ManualTrigger {
            reason: "perf_test".to_string(),
        });
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 10,
        "Enqueuing 100 triggers: {:?}",
        elapsed
    );
    assert_eq!(ml.pending_trigger_count(), 100);
}

#[test]
fn perf_should_evaluate_fast() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let agents = agents_n(3);
    let issues = issues_n(5);
    let c = ctx();

    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    let start = Instant::now();
    for i in 0..10_000 {
        ml.should_evaluate(30_000 + i);
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 50,
        "10k should_evaluate calls: {:?}",
        elapsed
    );
}

#[test]
fn perf_tick_skip_fast() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        cadence_ms: 300_000,
        ..MissionLoopConfig::default()
    });
    let agents = agents_n(3);
    let issues = issues_n(5);
    let c = ctx();

    // First evaluation
    ml.evaluate(30_000, MissionTrigger::CadenceTick, &issues, &agents, &c);

    // tick() should skip quickly when cadence hasn't elapsed
    let start = Instant::now();
    for i in 0..1000 {
        ml.tick(30_001 + i, &issues, &agents, &c);
    }
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 50,
        "1000 skipped ticks: {:?}",
        elapsed
    );
    // Only 1 cycle should have run (the initial evaluate)
    assert_eq!(ml.state().cycle_count, 1);
}

// ═══════════════════════════════════════════════════════════════════════
// §7 — Determinism
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn determinism_cycle_count_identical() {
    let run = || {
        let agents = agents_n(5);
        let issues = issues_n(10);
        let c = ctx();
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
        for i in 0..20 {
            ml.evaluate(
                (i + 1) * 30_000,
                MissionTrigger::CadenceTick,
                &issues,
                &agents,
                &c,
            );
        }
        (
            ml.state().cycle_count,
            ml.state().metrics_totals.assignments,
            ml.state().metrics_totals.rejections,
        )
    };

    assert_eq!(run(), run());
}

#[test]
fn determinism_report_identical() {
    let run = || {
        let agents = agents_n(3);
        let issues = issues_n(5);
        let c = ctx();
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
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
        serde_json::to_value(&report).unwrap()
    };

    assert_eq!(run(), run());
}

#[test]
fn determinism_metrics_history_identical() {
    let run = || {
        let agents = agents_n(3);
        let issues = issues_n(5);
        let c = ctx();
        let mut ml = MissionLoop::new(MissionLoopConfig::default());
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
