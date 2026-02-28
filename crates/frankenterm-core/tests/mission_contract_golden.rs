//! Mission system contract freeze tests (ft-1i2ge.5.7).
//!
//! Freeze the public API surface for mission loop types, operator report views,
//! operator override controls, and MCP mission tool schemas. Any test failure
//! here means the API contract has shifted and downstream consumers (CLI, Robot
//! Mode, MCP tools, agent coordinators) must be updated.
//!
//! # Contract coverage
//!
//! - MissionTrigger variants + serde
//! - MissionLoopConfig fields + defaults
//! - MissionDecision serde shape
//! - MissionLoopState serde shape (with override_state)
//! - OperatorStatusReport full field freeze
//! - OperatorOverrideKind variants + serde
//! - OperatorOverride struct fields + serde
//! - OperatorOverrideState lifecycle (activate/clear/evict)
//! - OverrideApplicationSummary serde shape
//! - AssignmentSet serde stability
//! - Determinism: identical inputs → identical JSON output
//! - Golden snapshot: MissionDecision JSON shape
//! - Golden snapshot: OperatorStatusReport JSON shape
//! - Golden snapshot: OperatorOverrideState JSON shape

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_loop::{
    MissionLoop, MissionLoopConfig, MissionLoopState, MissionTrigger, OperatorOverride,
    OperatorOverrideKind, OperatorOverrideState, OperatorStatusReport, OverrideApplicationSummary,
    PinnedAssignmentRecord, ReprioritizedBeadRecord,
};
use frankenterm_core::plan::{MissionAgentAvailability, MissionAgentCapabilityProfile};
use frankenterm_core::planner_features::{
    Assignment, AssignmentSet, PlannerExtractionContext, RejectedCandidate, RejectionReason,
    SolverConfig,
};

fn test_issue(id: &str, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Test bead {id}"),
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

fn test_agent(agent_id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: agent_id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// § MissionTrigger contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_mission_trigger_cadence_tick_serde() {
    let trigger = MissionTrigger::CadenceTick;
    let json = serde_json::to_string(&trigger).expect("serialize");
    let back: MissionTrigger = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(trigger, back);
    assert!(
        json.contains("cadence_tick"),
        "expected snake_case variant name"
    );
}

#[test]
fn contract_mission_trigger_bead_status_change_serde() {
    let trigger = MissionTrigger::BeadStatusChange {
        bead_id: "ft-test".to_string(),
    };
    let json = serde_json::to_string(&trigger).expect("serialize");
    let back: MissionTrigger = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(trigger, back);
    assert!(json.contains("bead_status_change"));
    assert!(json.contains("ft-test"));
}

#[test]
fn contract_mission_trigger_agent_availability_change_serde() {
    let trigger = MissionTrigger::AgentAvailabilityChange {
        agent_id: "agent-1".to_string(),
    };
    let json = serde_json::to_string(&trigger).expect("serialize");
    let back: MissionTrigger = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(trigger, back);
    assert!(json.contains("agent_availability_change"));
}

#[test]
fn contract_mission_trigger_manual_trigger_serde() {
    let trigger = MissionTrigger::ManualTrigger {
        reason: "operator request".to_string(),
    };
    let json = serde_json::to_string(&trigger).expect("serialize");
    let back: MissionTrigger = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(trigger, back);
    assert!(json.contains("manual_trigger"));
}

#[test]
fn contract_mission_trigger_external_signal_serde() {
    let trigger = MissionTrigger::ExternalSignal {
        source: "webhook".to_string(),
        payload: "{}".to_string(),
    };
    let json = serde_json::to_string(&trigger).expect("serialize");
    let back: MissionTrigger = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(trigger, back);
    assert!(json.contains("external_signal"));
}

// ─────────────────────────────────────────────────────────────────────────────
// § MissionLoopConfig defaults contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_mission_loop_config_defaults() {
    let config = MissionLoopConfig::default();
    assert_eq!(config.cadence_ms, 30_000, "default cadence = 30s");
    assert_eq!(config.max_trigger_batch, 10);
    assert!(!config.include_blocked_in_extraction);
}

#[test]
fn contract_mission_loop_config_serde_roundtrip() {
    let config = MissionLoopConfig::default();
    let json = serde_json::to_string(&config).expect("serialize");
    let back: MissionLoopConfig = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.cadence_ms, config.cadence_ms);
    assert_eq!(back.max_trigger_batch, config.max_trigger_batch);
}

// ─────────────────────────────────────────────────────────────────────────────
// § SolverConfig defaults contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_solver_config_defaults() {
    let config = SolverConfig::default();
    assert!(
        (config.min_score - 0.05).abs() < f64::EPSILON,
        "min_score default = 0.05"
    );
    assert_eq!(config.max_assignments, 10, "max_assignments default = 10");
    assert!(config.safety_gates.is_empty());
    assert!(config.conflicts.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// § OperatorOverrideKind contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_override_kind_pin_serde() {
    let kind = OperatorOverrideKind::Pin {
        bead_id: "b1".to_string(),
        target_agent: "agent-x".to_string(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    let back: OperatorOverrideKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(kind, back);
    assert!(json.contains("Pin"));
    assert!(json.contains("bead_id"));
    assert!(json.contains("target_agent"));
}

#[test]
fn contract_override_kind_exclude_serde() {
    let kind = OperatorOverrideKind::Exclude {
        bead_id: "b2".to_string(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    let back: OperatorOverrideKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(kind, back);
    assert!(json.contains("Exclude"));
}

#[test]
fn contract_override_kind_exclude_agent_serde() {
    let kind = OperatorOverrideKind::ExcludeAgent {
        agent_id: "agent-y".to_string(),
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    let back: OperatorOverrideKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(kind, back);
    assert!(json.contains("ExcludeAgent"));
}

#[test]
fn contract_override_kind_reprioritize_serde() {
    let kind = OperatorOverrideKind::Reprioritize {
        bead_id: "b3".to_string(),
        score_delta: -25,
    };
    let json = serde_json::to_string(&kind).expect("serialize");
    let back: OperatorOverrideKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(kind, back);
    assert!(json.contains("score_delta"));
}

// ─────────────────────────────────────────────────────────────────────────────
// § OperatorOverride struct contract
// ─────────────────────────────────────────────────────────────────────────────

fn make_test_override(id: &str, kind: OperatorOverrideKind) -> OperatorOverride {
    OperatorOverride {
        override_id: id.to_string(),
        kind,
        activated_by: "operator-test".to_string(),
        reason_code: "test.contract_freeze".to_string(),
        rationale: "Contract freeze test".to_string(),
        activated_at_ms: 1000,
        expires_at_ms: Some(5000),
        correlation_id: Some("corr-001".to_string()),
    }
}

#[test]
fn contract_operator_override_serde_roundtrip() {
    let ovr = make_test_override(
        "ovr-freeze-1",
        OperatorOverrideKind::Pin {
            bead_id: "freeze-bead".to_string(),
            target_agent: "freeze-agent".to_string(),
        },
    );
    let json = serde_json::to_string_pretty(&ovr).expect("serialize");
    let back: OperatorOverride = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.override_id, "ovr-freeze-1");
    assert_eq!(back.activated_by, "operator-test");
    assert_eq!(back.reason_code, "test.contract_freeze");
    assert_eq!(back.rationale, "Contract freeze test");
    assert_eq!(back.activated_at_ms, 1000);
    assert_eq!(back.expires_at_ms, Some(5000));
    assert_eq!(back.correlation_id, Some("corr-001".to_string()));
}

#[test]
fn contract_operator_override_required_fields_present_in_json() {
    let ovr = make_test_override(
        "field-check",
        OperatorOverrideKind::Exclude {
            bead_id: "x".to_string(),
        },
    );
    let json = serde_json::to_string(&ovr).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let obj = v.as_object().expect("should be object");

    let required = [
        "override_id",
        "kind",
        "activated_by",
        "reason_code",
        "rationale",
        "activated_at_ms",
        "expires_at_ms",
        "correlation_id",
    ];
    for field in &required {
        assert!(obj.contains_key(*field), "missing required field: {field}");
    }
}

#[test]
fn contract_operator_override_is_expired_boundary() {
    let ovr = make_test_override(
        "ttl-test",
        OperatorOverrideKind::Exclude {
            bead_id: "x".to_string(),
        },
    );
    // expires_at_ms = 5000
    assert!(!ovr.is_expired(4999));
    assert!(ovr.is_expired(5000));
    assert!(ovr.is_expired(5001));
}

// ─────────────────────────────────────────────────────────────────────────────
// § OperatorOverrideState contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_override_state_default_empty() {
    let state = OperatorOverrideState::default();
    assert!(state.active.is_empty());
    assert!(state.history.is_empty());
}

#[test]
fn contract_override_state_serde_roundtrip() {
    let mut state = OperatorOverrideState::default();
    state.activate(make_test_override(
        "s1",
        OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a1".to_string(),
        },
    ));
    state.activate(make_test_override(
        "s2",
        OperatorOverrideKind::Reprioritize {
            bead_id: "b2".to_string(),
            score_delta: 50,
        },
    ));
    state.clear("s1", 2000);

    let json = serde_json::to_string_pretty(&state).expect("serialize");
    let back: OperatorOverrideState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.active.len(), 1);
    assert_eq!(back.history.len(), 1);
    assert_eq!(back.active[0].override_id, "s2");
    assert_eq!(back.history[0].override_id, "s1");
}

#[test]
fn contract_override_state_accessors_return_correct_types() {
    let mut state = OperatorOverrideState::default();
    state.activate(make_test_override(
        "pin-1",
        OperatorOverrideKind::Pin {
            bead_id: "b1".to_string(),
            target_agent: "a1".to_string(),
        },
    ));
    state.activate(make_test_override(
        "excl-b",
        OperatorOverrideKind::Exclude {
            bead_id: "b2".to_string(),
        },
    ));
    state.activate(make_test_override(
        "excl-a",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a2".to_string(),
        },
    ));
    state.activate(make_test_override(
        "rep-1",
        OperatorOverrideKind::Reprioritize {
            bead_id: "b3".to_string(),
            score_delta: 100,
        },
    ));

    assert_eq!(state.active_pins().len(), 1);
    assert_eq!(state.excluded_bead_ids().len(), 1);
    assert_eq!(state.excluded_agent_ids().len(), 1);
    assert_eq!(state.reprioritize_deltas().len(), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// § OverrideApplicationSummary contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_override_application_summary_serde() {
    let summary = OverrideApplicationSummary {
        excluded_beads: vec!["b1".to_string()],
        excluded_agents: vec!["a1".to_string()],
        pinned_assignments: vec![PinnedAssignmentRecord {
            bead_id: "b2".to_string(),
            agent_id: "a2".to_string(),
            override_id: "ovr-1".to_string(),
        }],
        reprioritized_beads: vec![ReprioritizedBeadRecord {
            bead_id: "b3".to_string(),
            original_score: 0.5,
            adjusted_score: 1.0,
            delta: 50,
        }],
        expired_overrides: 2,
    };
    let json = serde_json::to_string_pretty(&summary).expect("serialize");
    let back: OverrideApplicationSummary = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.excluded_beads, vec!["b1"]);
    assert_eq!(back.excluded_agents, vec!["a1"]);
    assert_eq!(back.pinned_assignments.len(), 1);
    assert_eq!(back.reprioritized_beads.len(), 1);
    assert_eq!(back.expired_overrides, 2);
}

#[test]
fn contract_override_application_summary_required_fields() {
    let summary = OverrideApplicationSummary::default();
    let json = serde_json::to_string(&summary).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let obj = v.as_object().expect("object");
    let required = [
        "excluded_beads",
        "excluded_agents",
        "pinned_assignments",
        "reprioritized_beads",
        "expired_overrides",
    ];
    for field in &required {
        assert!(obj.contains_key(*field), "missing field: {field}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// § MissionLoopState contract (override_state field present)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_mission_loop_state_has_override_state() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let state = ml.state();
    // override_state field must exist and be default-empty.
    assert!(state.override_state.active.is_empty());
    assert!(state.override_state.history.is_empty());
    assert!(state.last_override_summary.is_none());
}

#[test]
fn contract_mission_loop_state_serde_with_overrides() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    ml.apply_override(make_test_override(
        "contract-pin",
        OperatorOverrideKind::Pin {
            bead_id: "cb1".to_string(),
            target_agent: "ca1".to_string(),
        },
    ))
    .expect("apply");

    let state = ml.state();
    let json = serde_json::to_string(state).expect("serialize");
    let back: MissionLoopState = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.override_state.active.len(), 1);
    assert_eq!(back.override_state.active[0].override_id, "contract-pin");
}

// ─────────────────────────────────────────────────────────────────────────────
// § AssignmentSet contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_assignment_set_serde_roundtrip() {
    let set = AssignmentSet {
        assignments: vec![Assignment {
            bead_id: "b1".to_string(),
            agent_id: "a1".to_string(),
            score: 0.85,
            rank: 1,
        }],
        rejected: vec![RejectedCandidate {
            bead_id: "b2".to_string(),
            score: 0.02,
            reasons: vec![RejectionReason::BelowScoreThreshold],
        }],
        solver_config: SolverConfig::default(),
    };
    let json = serde_json::to_string_pretty(&set).expect("serialize");
    let back: AssignmentSet = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.assignments.len(), 1);
    assert_eq!(back.rejected.len(), 1);
    assert_eq!(back.assignments[0].bead_id, "b1");
    assert_eq!(back.assignments[0].rank, 1);
}

#[test]
fn contract_rejection_reason_variants_serde() {
    let reasons = vec![
        RejectionReason::BelowScoreThreshold,
        RejectionReason::NoCapacity,
        RejectionReason::SafetyGateDenied {
            gate_name: "test.gate".to_string(),
        },
        RejectionReason::ConflictWithAssigned {
            conflicting_bead_id: "b-conflict".to_string(),
        },
    ];
    for reason in &reasons {
        let json = serde_json::to_string(reason).expect("serialize");
        let back: RejectionReason = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*reason, back, "roundtrip failed for {json}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// § OperatorStatusReport golden snapshot
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn golden_operator_report_idle_state_json_shape() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let report = ml.generate_operator_report(None, None);
    let json = serde_json::to_string_pretty(&report).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let obj = v.as_object().expect("object");

    // Top-level sections must exist.
    let required_sections = [
        "status",
        "assignment_table",
        "health",
        "conflicts",
        "event_summary",
        "latest_explanations",
    ];
    for section in &required_sections {
        assert!(obj.contains_key(*section), "missing section: {section}");
    }

    // Status section fields.
    let status = obj["status"].as_object().expect("status object");
    for field in &[
        "cycle_count",
        "last_evaluation_ms",
        "total_assignments",
        "total_rejections",
        "pending_trigger_count",
        "phase_label",
    ] {
        assert!(status.contains_key(*field), "status missing field: {field}");
    }

    // Health section fields.
    let health = obj["health"].as_object().expect("health object");
    for field in &[
        "throughput_assignments_per_minute",
        "unblock_velocity_per_minute",
        "conflict_rate",
        "planner_churn_rate",
        "policy_deny_rate",
        "avg_evaluation_latency_ms",
        "overall",
    ] {
        assert!(health.contains_key(*field), "health missing field: {field}");
    }

    // Conflicts section fields.
    let conflicts = obj["conflicts"].as_object().expect("conflicts object");
    for field in &[
        "total_detected",
        "total_auto_resolved",
        "pending_manual",
        "recent_conflicts",
    ] {
        assert!(
            conflicts.contains_key(*field),
            "conflicts missing field: {field}"
        );
    }

    // Event summary fields.
    let events = obj["event_summary"].as_object().expect("events object");
    for field in &["retained_events", "total_emitted", "by_phase", "by_kind"] {
        assert!(events.contains_key(*field), "events missing field: {field}");
    }
}

#[test]
fn golden_operator_report_idle_values() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let report = ml.generate_operator_report(None, None);

    assert_eq!(report.status.cycle_count, 0);
    assert_eq!(report.status.phase_label, "idle");
    assert!(report.assignment_table.is_empty());
    assert_eq!(report.health.overall, "idle");
    assert_eq!(report.conflicts.total_detected, 0);
    assert_eq!(report.event_summary.total_emitted, 0);
    assert!(report.latest_explanations.is_empty());
}

#[test]
fn golden_operator_report_json_roundtrip() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let report = ml.generate_operator_report(None, None);
    let json = serde_json::to_string(&report).expect("serialize");
    let back: OperatorStatusReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.status.cycle_count, report.status.cycle_count);
    assert_eq!(back.status.phase_label, report.status.phase_label);
    assert_eq!(back.health.overall, report.health.overall);
}

// ─────────────────────────────────────────────────────────────────────────────
// § MissionDecision golden snapshot
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn golden_mission_decision_json_shape() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![test_issue("golden-b1", 1)];
    let agents = vec![test_agent("golden-agent")];
    let ctx = PlannerExtractionContext::default();
    let decision = ml.evaluate(10_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    let json = serde_json::to_string_pretty(&decision).expect("serialize");
    let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
    let obj = v.as_object().expect("object");

    let required = [
        "cycle_id",
        "timestamp_ms",
        "trigger",
        "assignment_set",
        "extraction_summary",
        "scorer_summary",
    ];
    for field in &required {
        assert!(obj.contains_key(*field), "decision missing field: {field}");
    }

    // Assignment set sub-fields.
    let aset = obj["assignment_set"].as_object().expect("assignment_set");
    assert!(aset.contains_key("assignments"));
    assert!(aset.contains_key("rejected"));
    assert!(aset.contains_key("solver_config"));

    // Extraction summary sub-fields.
    let ext = obj["extraction_summary"]
        .as_object()
        .expect("extraction_summary");
    assert!(ext.contains_key("total_candidates"));
    assert!(ext.contains_key("ready_candidates"));

    // Scorer summary sub-fields.
    let scr = obj["scorer_summary"].as_object().expect("scorer_summary");
    assert!(scr.contains_key("scored_count"));
    assert!(scr.contains_key("above_threshold_count"));
}

// ─────────────────────────────────────────────────────────────────────────────
// § Determinism: identical inputs → identical JSON
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_determinism_decision_repeat() {
    let issues = vec![test_issue("det-a", 2), test_issue("det-b", 1)];
    let agents = vec![test_agent("det-agent")];
    let ctx = PlannerExtractionContext::default();

    let mut ml1 = MissionLoop::new(MissionLoopConfig::default());
    let d1 = ml1.evaluate(5000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    let mut ml2 = MissionLoop::new(MissionLoopConfig::default());
    let d2 = ml2.evaluate(5000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    let j1 = serde_json::to_string(&d1).expect("j1");
    let j2 = serde_json::to_string(&d2).expect("j2");
    assert_eq!(
        j1, j2,
        "decisions must be deterministic for identical inputs"
    );
}

#[test]
fn contract_determinism_report_repeat() {
    let ml = MissionLoop::new(MissionLoopConfig::default());
    let r1 = ml.generate_operator_report(None, None);
    let r2 = ml.generate_operator_report(None, None);
    let j1 = serde_json::to_string(&r1).expect("j1");
    let j2 = serde_json::to_string(&r2).expect("j2");
    assert_eq!(j1, j2, "reports must be deterministic");
}

// ─────────────────────────────────────────────────────────────────────────────
// § MissionLoop override API contract
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_apply_override_returns_ok_for_unique_id() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let result = ml.apply_override(make_test_override(
        "api-1",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    ));
    assert!(result.is_ok());
    assert_eq!(ml.active_overrides().len(), 1);
}

#[test]
fn contract_apply_override_rejects_duplicate_id() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    ml.apply_override(make_test_override(
        "dup",
        OperatorOverrideKind::Exclude {
            bead_id: "b1".to_string(),
        },
    ))
    .unwrap();
    let err = ml
        .apply_override(make_test_override(
            "dup",
            OperatorOverrideKind::Exclude {
                bead_id: "b2".to_string(),
            },
        ))
        .unwrap_err();
    assert!(
        err.contains("already active"),
        "error should mention 'already active'"
    );
}

#[test]
fn contract_clear_override_returns_true_for_existing() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    ml.apply_override(make_test_override(
        "to-clear",
        OperatorOverrideKind::ExcludeAgent {
            agent_id: "a1".to_string(),
        },
    ))
    .unwrap();
    assert!(ml.clear_override("to-clear", 2000));
    assert!(ml.active_overrides().is_empty());
}

#[test]
fn contract_clear_override_returns_false_for_missing() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    assert!(!ml.clear_override("nonexistent", 1000));
}

// ─────────────────────────────────────────────────────────────────────────────
// § Backward compatibility: empty override_state deserializes from old JSON
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn contract_backward_compat_state_without_override_fields() {
    // Simulate JSON from before override_state was added.
    let old_json = r#"{
        "cycle_count": 3,
        "last_evaluation_ms": 1000,
        "pending_triggers": [],
        "last_decision": null,
        "total_assignments_made": 5,
        "total_rejections": 1,
        "total_conflicts_detected": 0,
        "total_conflicts_auto_resolved": 0
    }"#;
    let state: MissionLoopState = serde_json::from_str(old_json).expect("deserialize old format");
    assert_eq!(state.cycle_count, 3);
    assert!(state.override_state.active.is_empty());
    assert!(state.last_override_summary.is_none());
}
