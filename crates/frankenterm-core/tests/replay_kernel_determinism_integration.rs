//! Cross-module kernel determinism integration tests (ft-og6q6.3.6).
//!
//! Validates that VirtualClock, ReplayScheduler, PaneMergeResolver,
//! SideEffectBarrier, and ProvenanceEmitter work together correctly
//! across module boundaries.
//!
//! Test taxonomy:
//! - I-01: Single-pane replay roundtrip
//! - I-02: Multi-pane interleaved replay
//! - I-03: Checkpoint/resume equivalence
//! - I-04: Side-effect isolation verification
//! - I-05: Clock anomaly detection and recovery
//! - I-06..I-24: Additional cross-module integration scenarios

use std::collections::HashMap;

use frankenterm_core::event_id::{RecorderMergeKey, StreamKind};
use frankenterm_core::policy::ActionKind;
use frankenterm_core::recorder_replay::{
    ReplayConfig, ReplayEquivalenceLevel, ReplayScheduler, VirtualClock, VirtualClockSnapshot,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
};
use frankenterm_core::replay_merge::{
    ClockAnomalyAnnotation, MergeConfig, MergeEvent, MergeEventPayload, PaneMergeResolver,
};
use frankenterm_core::replay_provenance::{
    AuditEntryParams, DecisionExplanationTrace, DecisionType, ExplanationLink,
    ExplanationTraceCollector, ProvenanceConfig, ProvenanceRecordParams, ProvenanceVerbosity,
    REPLAY_AUDIT_GENESIS, ReplayAuditTrail, ReplayProvenanceEmitter,
};
use frankenterm_core::replay_side_effect_barrier::{
    CounterfactualBarrier, EffectRequest, EffectType, LiveBarrier, OverrideRule, ReplayBarrier,
    SideEffectBarrier, SideEffectLog,
};

// ── Helpers ──────────────────────────────────────────────────────────────

fn make_event(
    pane_id: u64,
    sequence: u64,
    occurred_at_ms: u64,
    recorded_at_ms: u64,
    text: &str,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-{pane_id}-{sequence}-{recorded_at_ms}"),
        pane_id,
        session_id: Some("session-int".to_string()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms,
        recorded_at_ms,
        sequence,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload: RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
    }
}

fn make_merge_event(
    recorded_at_ms: u64,
    pane_id: u64,
    sequence: u64,
    event_type: &str,
) -> MergeEvent {
    MergeEvent {
        merge_key: RecorderMergeKey {
            recorded_at_ms,
            pane_id,
            stream_kind: StreamKind::Ingress,
            sequence,
            event_id: format!("mevt-{pane_id}-{sequence}"),
        },
        source_position: sequence as usize,
        source_pane_id: pane_id,
        is_gap_marker: false,
        clock_anomaly: None,
        payload: MergeEventPayload {
            event_type: event_type.to_string(),
            data: serde_json::json!({"text": format!("data-{pane_id}-{sequence}")}),
        },
    }
}

fn make_effect_request(effect_type: EffectType, pane_id: u64, payload: &str) -> EffectRequest {
    EffectRequest {
        timestamp_ms: 1000,
        effect_type,
        pane_id: Some(pane_id),
        payload: payload.to_string(),
        caller: "integration-test".to_string(),
        action_kind: ActionKind::SendText,
        metadata: HashMap::new(),
    }
}

// ── I-01: Single-pane replay roundtrip ───────────────────────────────────

#[test]
fn i01_single_pane_replay_roundtrip() {
    let events = vec![
        make_event(1, 0, 1000, 1000, "ls -la"),
        make_event(1, 1, 2000, 2000, "cd /tmp"),
        make_event(1, 2, 3000, 3000, "echo hello"),
    ];

    let mut scheduler = ReplayScheduler::new(events.clone(), ReplayConfig::instant()).unwrap();

    let mut replayed = Vec::new();
    while let Some(step) = scheduler.next_step() {
        replayed.push(step);
    }

    assert_eq!(replayed.len(), 3, "all 3 events should be replayed");

    // Verify ordering matches input
    for (i, step) in replayed.iter().enumerate() {
        assert_eq!(step.cursor, i);
    }
}

// ── I-02: Multi-pane interleaved replay ──────────────────────────────────

#[test]
fn i02_multi_pane_interleaved_replay() {
    // Two panes with interleaved timestamps
    let events = vec![
        make_event(1, 0, 1000, 1000, "pane1-first"),
        make_event(2, 0, 1500, 1500, "pane2-first"),
        make_event(1, 1, 2000, 2000, "pane1-second"),
        make_event(2, 1, 2500, 2500, "pane2-second"),
    ];

    let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();

    let mut step_panes = Vec::new();
    while let Some(step) = scheduler.next_step() {
        step_panes.push(step.merge_pane_id);
    }

    assert_eq!(step_panes.len(), 4);
    // Events are merged by recorded_at_ms, so should maintain temporal order
    assert_eq!(step_panes[0], 1); // 1000ms
    assert_eq!(step_panes[1], 2); // 1500ms
    assert_eq!(step_panes[2], 1); // 2000ms
    assert_eq!(step_panes[3], 2); // 2500ms
}

// ── I-03: Checkpoint/resume equivalence ──────────────────────────────────

#[test]
fn i03_checkpoint_resume_equivalence() {
    let events = vec![
        make_event(1, 0, 1000, 1000, "step-0"),
        make_event(1, 1, 2000, 2000, "step-1"),
        make_event(1, 2, 3000, 3000, "step-2"),
        make_event(1, 3, 4000, 4000, "step-3"),
        make_event(1, 4, 5000, 5000, "step-4"),
    ];

    // Run baseline to completion
    let mut baseline = ReplayScheduler::new(events.clone(), ReplayConfig::instant()).unwrap();
    let mut baseline_decisions = Vec::new();
    while let Some(step) = baseline.next_step() {
        baseline_decisions.push(step.decision.decision_id.clone());
    }

    // Run halfway, checkpoint, then resume
    let mut first_half = ReplayScheduler::new(events.clone(), ReplayConfig::instant()).unwrap();
    let mut partial = Vec::new();
    for _ in 0..3 {
        if let Some(step) = first_half.next_step() {
            partial.push(step.decision.decision_id.clone());
        }
    }
    let checkpoint = first_half.checkpoint();

    let mut second_half = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
    second_half.resume(checkpoint).unwrap();
    while let Some(step) = second_half.next_step() {
        partial.push(step.decision.decision_id.clone());
    }

    assert_eq!(
        baseline_decisions, partial,
        "checkpoint+resume must produce identical decision sequence"
    );
}

// ── I-04: Side-effect isolation in replay mode ───────────────────────────

#[test]
fn i04_side_effect_isolation_replay_mode() {
    let replay_barrier = ReplayBarrier::new();

    // In replay mode, side effects should be blocked
    let request = make_effect_request(EffectType::SendKeys, 1, "dangerous-command");
    let outcome = replay_barrier.process(&request);

    assert!(!outcome.executed, "replay barrier should block execution");

    // Live mode should allow
    let live_barrier = LiveBarrier::new();
    let outcome = live_barrier.process(&request);
    assert!(outcome.executed, "live barrier should allow execution");

    // Verify barrier mode names
    assert_eq!(replay_barrier.mode_name(), "replay");
    assert_eq!(live_barrier.mode_name(), "live");
}

// ── I-05: Clock anomaly detection and recovery ───────────────────────────

#[test]
fn i05_clock_anomaly_detection() {
    let mut resolver = PaneMergeResolver::new(MergeConfig {
        future_skew_threshold_ms: 100,
        include_gap_markers: true,
    });

    // Normal event
    let normal = make_merge_event(1000, 1, 0, "ingress_text");

    // Anomalous event: timestamp jumps backward
    let mut anomalous = make_merge_event(500, 1, 1, "ingress_text");
    anomalous.clock_anomaly = Some(ClockAnomalyAnnotation {
        is_anomaly: true,
        reason: Some("backward clock jump".to_string()),
    });

    resolver.add_pane_stream(1, vec![normal, anomalous]);
    let merged = resolver.merge();

    // Both events should appear (anomalies are annotated, not dropped)
    assert_eq!(merged.len(), 2);
    let stats = resolver.stats();
    assert_eq!(stats.anomaly_count, 1);
}

// ── I-06: Virtual clock speed affects scheduler delay ────────────────────

#[test]
fn i06_virtual_clock_speed_control() {
    let mut clock_1x = VirtualClock::new(1.0).unwrap();
    let mut clock_2x = VirtualClock::new(2.0).unwrap();
    let mut clock_inf = VirtualClock::new(f64::INFINITY).unwrap();

    let event = make_event(1, 0, 1000, 1000, "test");
    let event2 = make_event(1, 1, 3000, 3000, "test2");

    // First event initializes the clock
    let _ = clock_1x.advance_to_event(&event, 10000);
    let _ = clock_2x.advance_to_event(&event, 10000);
    let _ = clock_inf.advance_to_event(&event, 10000);

    let delay_1x = clock_1x.advance_to_event(&event2, 10000);
    let delay_2x = clock_2x.advance_to_event(&event2, 10000);
    let delay_inf = clock_inf.advance_to_event(&event2, 10000);

    // 2x speed should halve the delay
    assert_eq!(delay_1x.as_millis(), 2000);
    assert_eq!(delay_2x.as_millis(), 1000);
    assert_eq!(delay_inf.as_millis(), 0);
}

// ── I-07: Merge resolver + scheduler combined ────────────────────────────

#[test]
fn i07_merge_then_schedule_pipeline() {
    // Step 1: Merge events from two panes
    let mut resolver = PaneMergeResolver::with_defaults();

    resolver.add_pane_stream(
        1,
        vec![
            make_merge_event(1000, 1, 0, "ingress_text"),
            make_merge_event(3000, 1, 1, "ingress_text"),
        ],
    );
    resolver.add_pane_stream(
        2,
        vec![
            make_merge_event(2000, 2, 0, "ingress_text"),
            make_merge_event(4000, 2, 1, "ingress_text"),
        ],
    );

    let merged = resolver.merge();
    assert_eq!(merged.len(), 4);

    // Verify merge order: 1000, 2000, 3000, 4000
    let timestamps: Vec<u64> = merged.iter().map(|e| e.merge_key.recorded_at_ms).collect();
    assert_eq!(timestamps, vec![1000, 2000, 3000, 4000]);

    // Step 2: Create RecorderEvents matching merge order
    let events: Vec<RecorderEvent> = timestamps
        .iter()
        .enumerate()
        .map(|(i, &ts)| make_event(((i % 2) + 1) as u64, i as u64, ts, ts, "merged"))
        .collect();

    // Step 3: Schedule replay
    let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
    let mut replayed_ts = Vec::new();
    while let Some(step) = scheduler.next_step() {
        replayed_ts.push(step.merge_recorded_at_ms);
    }

    assert_eq!(replayed_ts, vec![1000, 2000, 3000, 4000]);
}

// ── I-08: Provenance tracks all decisions ────────────────────────────────

#[test]
fn i08_provenance_tracks_scheduler_decisions() {
    let emitter = ReplayProvenanceEmitter::with_defaults("run-i08".to_string());

    let events = vec![
        make_event(1, 0, 1000, 1000, "step-a"),
        make_event(1, 1, 2000, 2000, "step-b"),
        make_event(1, 2, 3000, 3000, "step-c"),
    ];

    let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();

    while let Some(step) = scheduler.next_step() {
        emitter.record(ProvenanceRecordParams {
            event_id: step.merge_event_id.clone(),
            decision_type: DecisionType::PatternMatch,
            rule_id: format!("rule-{}", step.cursor),
            definition_hash: "hash-abc".to_string(),
            output_summary: format!("processed cursor {}", step.cursor),
            wall_clock_ms: step.merge_recorded_at_ms,
            virtual_clock_ms: step.clock.occurred_at_ms,
            input_data: serde_json::json!({"cursor": step.cursor}),
            event_context: None,
        });
    }

    assert_eq!(emitter.len(), 3);
    let entries = emitter.entries();
    assert_eq!(entries[0].rule_id, "rule-0");
    assert_eq!(entries[1].rule_id, "rule-1");
    assert_eq!(entries[2].rule_id, "rule-2");
}

// ── I-09: Provenance JSONL roundtrip ─────────────────────────────────────

#[test]
fn i09_provenance_jsonl_serde_roundtrip() {
    let emitter = ReplayProvenanceEmitter::new(
        "run-i09".to_string(),
        ProvenanceConfig {
            verbosity: ProvenanceVerbosity::Verbose,
            max_memory_entries: 100,
        },
    );

    for i in 0..5 {
        emitter.record(ProvenanceRecordParams {
            event_id: format!("evt-{i}"),
            decision_type: DecisionType::WorkflowStep,
            rule_id: format!("rule-{i}"),
            definition_hash: format!("hash-{i}"),
            output_summary: format!("output-{i}"),
            wall_clock_ms: 1000 * (i as u64 + 1),
            virtual_clock_ms: 1000 * (i as u64 + 1),
            input_data: serde_json::json!({"idx": i}),
            event_context: Some(serde_json::json!({"source": "test"})),
        });
    }

    let jsonl = emitter.to_jsonl();
    let restored = ReplayProvenanceEmitter::from_jsonl(&jsonl).unwrap();
    assert_eq!(restored.len(), 5);

    for (orig, rest) in emitter.entries().iter().zip(restored.iter()) {
        assert_eq!(orig.event_id, rest.event_id);
        assert_eq!(orig.rule_id, rest.rule_id);
    }
}

// ── I-10: Audit trail chain integrity ────────────────────────────────────

#[test]
fn i10_audit_trail_chain_integrity() {
    let trail = ReplayAuditTrail::new();

    trail.append(AuditEntryParams {
        replay_run_id: "run-1".to_string(),
        actor: "test-agent".to_string(),
        started_at_ms: 1000,
        completed_at_ms: 2000,
        artifact_ref: "art-1".to_string(),
        override_ref: None,
        decision_count: 10,
        anomaly_count: 0,
    });

    trail.append(AuditEntryParams {
        replay_run_id: "run-2".to_string(),
        actor: "test-agent".to_string(),
        started_at_ms: 3000,
        completed_at_ms: 4000,
        artifact_ref: "art-2".to_string(),
        override_ref: Some("override-1".to_string()),
        decision_count: 15,
        anomaly_count: 1,
    });

    let verification = trail.verify();
    assert!(verification.chain_intact, "audit chain should be intact");
    assert_eq!(verification.total_entries, 2);
    assert!(verification.missing_ordinals.is_empty());
}

// ── I-11: Counterfactual barrier overrides ───────────────────────────────

#[test]
fn i11_counterfactual_barrier_overrides() {
    let overrides = vec![OverrideRule {
        effect_type: EffectType::SendKeys,
        pane_id: Some(1),
        payload_contains: Some("rm -rf".to_string()),
        replacement_payload: "echo 'blocked'".to_string(),
        description: "block dangerous commands".to_string(),
    }];

    let barrier = CounterfactualBarrier::new(overrides);

    // Matching request should be overridden
    let dangerous = make_effect_request(EffectType::SendKeys, 1, "rm -rf /");
    let outcome = barrier.process(&dangerous);
    assert!(outcome.overridden, "dangerous command should be overridden");

    // Non-matching request should pass through
    let safe = make_effect_request(EffectType::SendKeys, 1, "ls -la");
    let outcome = barrier.process(&safe);
    assert!(!outcome.overridden, "safe command should not be overridden");
}

// ── I-12: Side-effect log captures all effects ───────────────────────────

#[test]
fn i12_side_effect_log_capture() {
    let log = SideEffectLog::new();
    let barrier = ReplayBarrier::with_log(log);

    // Process multiple requests
    for i in 0..5 {
        let request = make_effect_request(EffectType::SendKeys, i, &format!("cmd-{i}"));
        barrier.process(&request);
    }

    let effect_log = barrier.log().unwrap();
    assert_eq!(effect_log.len(), 5, "all 5 effects should be logged");
}

// ── I-13: Explanation trace depth tracking ───────────────────────────────

#[test]
fn i13_explanation_trace_depth() {
    let collector = ExplanationTraceCollector::new();

    // Single-link trace (no mismatch — same hashes)
    collector.add(DecisionExplanationTrace::single(
        0,
        "evt-1".to_string(),
        "rule-pattern".to_string(),
        "hash-same".to_string(),
        "hash-same".to_string(),
        "matched".to_string(),
    ));

    // Multi-link trace (counterfactual — hashes differ)
    let mut multi_trace = DecisionExplanationTrace::single(
        1,
        "evt-2".to_string(),
        "rule-policy".to_string(),
        "hash-r2".to_string(),
        "hash-a2".to_string(),
        "diverged".to_string(),
    );
    multi_trace.push_link(ExplanationLink {
        triggering_event_id: "evt-2b".to_string(),
        rule_id: "rule-override".to_string(),
        replay_definition_hash: "hash-r3".to_string(),
        artifact_definition_hash: "hash-a3".to_string(),
        definition_mismatch: true,
        decision_output: "overridden".to_string(),
    });
    collector.add(multi_trace);

    assert_eq!(collector.len(), 2);
    assert_eq!(collector.counterfactual_count(), 1);

    let traces = collector.traces();
    let mut depths: Vec<usize> = traces.iter().map(|t| t.depth()).collect();
    depths.sort();
    assert_eq!(depths, vec![1, 2]);
}

// ── I-14: Deterministic replay produces identical decision IDs ───────────

#[test]
fn i14_deterministic_replay_decision_ids() {
    let events = vec![
        make_event(1, 0, 1000, 1000, "alpha"),
        make_event(2, 0, 1500, 1500, "beta"),
        make_event(1, 1, 2000, 2000, "gamma"),
    ];

    let collect_ids = |events: Vec<RecorderEvent>| -> Vec<String> {
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::instant()).unwrap();
        let mut ids = Vec::new();
        while let Some(step) = scheduler.next_step() {
            ids.push(step.decision.decision_id.clone());
        }
        ids
    };

    let run1 = collect_ids(events.clone());
    let run2 = collect_ids(events);

    assert_eq!(
        run1, run2,
        "identical input must produce identical decision IDs"
    );
}

// ── I-15: Pane filter restricts replay scope ─────────────────────────────

#[test]
fn i15_pane_filter_restricts_scope() {
    let events = vec![
        make_event(1, 0, 1000, 1000, "pane1"),
        make_event(2, 0, 1500, 1500, "pane2"),
        make_event(3, 0, 2000, 2000, "pane3"),
        make_event(1, 1, 2500, 2500, "pane1-again"),
    ];

    let config = ReplayConfig::instant().with_panes(vec![1, 3]);
    let mut scheduler = ReplayScheduler::new(events, config).unwrap();

    let mut seen_panes = Vec::new();
    while let Some(step) = scheduler.next_step() {
        seen_panes.push(step.merge_pane_id);
    }

    assert!(seen_panes.iter().all(|&p| p == 1 || p == 3));
    assert!(!seen_panes.contains(&2));
}

// ── I-16: Merge stats consistent with input ──────────────────────────────

#[test]
fn i16_merge_stats_consistency() {
    let mut resolver = PaneMergeResolver::with_defaults();

    resolver.add_pane_stream(
        1,
        vec![
            make_merge_event(1000, 1, 0, "ingress_text"),
            make_merge_event(2000, 1, 1, "ingress_text"),
        ],
    );
    resolver.add_pane_stream(2, vec![make_merge_event(1500, 2, 0, "ingress_text")]);

    // pane_count is tracked before merge (merge drains pane_streams)
    assert_eq!(resolver.pane_count(), 2);
    assert_eq!(resolver.total_events(), 3);

    let merged = resolver.merge();
    assert_eq!(merged.len(), 3);
    let stats = resolver.stats();
    assert_eq!(stats.total_events, 3);
}

// ── I-17: Provenance verbosity filtering ─────────────────────────────────

#[test]
fn i17_provenance_verbosity_levels() {
    for verbosity in [
        ProvenanceVerbosity::Minimal,
        ProvenanceVerbosity::Standard,
        ProvenanceVerbosity::Verbose,
    ] {
        let emitter = ReplayProvenanceEmitter::new(
            format!("run-{:?}", verbosity),
            ProvenanceConfig {
                verbosity,
                max_memory_entries: 100,
            },
        );

        emitter.record(ProvenanceRecordParams {
            event_id: "evt-1".to_string(),
            decision_type: DecisionType::PatternMatch,
            rule_id: "rule-1".to_string(),
            definition_hash: "hash-1".to_string(),
            output_summary: "output-1".to_string(),
            wall_clock_ms: 1000,
            virtual_clock_ms: 1000,
            input_data: serde_json::json!({"key": "value"}),
            event_context: Some(serde_json::json!({"ctx": true})),
        });

        assert_eq!(emitter.len(), 1);
    }
}

// ── I-18: Audit chain genesis hash ───────────────────────────────────────

#[test]
fn i18_audit_chain_genesis() {
    let trail = ReplayAuditTrail::new();

    trail.append(AuditEntryParams {
        replay_run_id: "genesis-test".to_string(),
        actor: "agent".to_string(),
        started_at_ms: 0,
        completed_at_ms: 100,
        artifact_ref: "art-0".to_string(),
        override_ref: None,
        decision_count: 1,
        anomaly_count: 0,
    });

    let entries = trail.entries();
    assert_eq!(entries[0].prev_entry_hash, REPLAY_AUDIT_GENESIS);
}

// ── I-19: Multiple barrier modes with shared log ─────────────────────────

#[test]
fn i19_barrier_mode_transitions() {
    // Simulate a session that transitions from live → replay → counterfactual
    let log = SideEffectLog::new();

    // Phase 1: Live capture
    let live = LiveBarrier::new();
    let req = make_effect_request(EffectType::SendKeys, 1, "initial");
    let outcome = live.process(&req);
    assert!(outcome.executed);

    // Phase 2: Replay (blocked)
    let replay = ReplayBarrier::with_log(log.clone());
    let req = make_effect_request(EffectType::SendKeys, 1, "replayed");
    let outcome = replay.process(&req);
    assert!(!outcome.executed);

    // Phase 3: Counterfactual (overrides)
    let cf = CounterfactualBarrier::new(vec![OverrideRule {
        effect_type: EffectType::SendKeys,
        pane_id: None,
        payload_contains: None,
        replacement_payload: "noop".to_string(),
        description: "override all".to_string(),
    }]);
    let req = make_effect_request(EffectType::SendKeys, 1, "counterfactual");
    let outcome = cf.process(&req);
    assert!(outcome.overridden);
}

// ── I-20: Scheduler with equivalence levels ──────────────────────────────

#[test]
fn i20_equivalence_levels() {
    let events = vec![
        make_event(1, 0, 1000, 1000, "test"),
        make_event(1, 1, 2000, 2000, "test2"),
    ];

    for level in [
        ReplayEquivalenceLevel::Structural,
        ReplayEquivalenceLevel::Decision,
        ReplayEquivalenceLevel::Full,
    ] {
        let config = ReplayConfig::instant().with_equivalence_level(level);
        let mut scheduler = ReplayScheduler::new(events.clone(), config).unwrap();
        let mut count = 0;
        while scheduler.next_step().is_some() {
            count += 1;
        }
        assert_eq!(
            count, 2,
            "all events should replay at every equivalence level"
        );
    }
}

// ── I-21: Clock snapshot serialization roundtrip ─────────────────────────

#[test]
fn i21_clock_snapshot_serde() {
    let snapshot = VirtualClockSnapshot {
        occurred_at_ms: 42000,
        recorded_at_ms: 42500,
        initialized: true,
    };

    let json = serde_json::to_string(&snapshot).unwrap();
    let restored: VirtualClockSnapshot = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.occurred_at_ms, 42000);
    assert_eq!(restored.recorded_at_ms, 42500);
    assert!(restored.initialized);
}

// ── I-22: Provenance entries_of_type filter ──────────────────────────────

#[test]
fn i22_provenance_type_filter() {
    let emitter = ReplayProvenanceEmitter::with_defaults("run-i22".to_string());

    let types = [
        DecisionType::PatternMatch,
        DecisionType::WorkflowStep,
        DecisionType::PolicyEvaluation,
        DecisionType::PatternMatch,
        DecisionType::SideEffectBarrier,
    ];

    for (i, dt) in types.iter().enumerate() {
        emitter.record(ProvenanceRecordParams {
            event_id: format!("evt-{i}"),
            decision_type: *dt,
            rule_id: format!("rule-{i}"),
            definition_hash: "hash".to_string(),
            output_summary: "out".to_string(),
            wall_clock_ms: i as u64 * 1000,
            virtual_clock_ms: i as u64 * 1000,
            input_data: serde_json::json!(null),
            event_context: None,
        });
    }

    let pattern_matches = emitter.entries_of_type(DecisionType::PatternMatch);
    assert_eq!(pattern_matches.len(), 2);

    let workflow_steps = emitter.entries_of_type(DecisionType::WorkflowStep);
    assert_eq!(workflow_steps.len(), 1);

    let barrier_entries = emitter.entries_of_type(DecisionType::SideEffectBarrier);
    assert_eq!(barrier_entries.len(), 1);
}

// ── I-23: Audit trail JSONL roundtrip ────────────────────────────────────

#[test]
fn i23_audit_trail_jsonl_roundtrip() {
    let trail = ReplayAuditTrail::new();

    for i in 0..3 {
        trail.append(AuditEntryParams {
            replay_run_id: format!("run-{i}"),
            actor: "agent".to_string(),
            started_at_ms: i * 1000,
            completed_at_ms: (i + 1) * 1000,
            artifact_ref: format!("art-{i}"),
            override_ref: None,
            decision_count: 10 + i,
            anomaly_count: 0,
        });
    }

    let jsonl = trail.to_jsonl();
    let restored = ReplayAuditTrail::from_jsonl(&jsonl).unwrap();

    assert_eq!(restored.len(), 3);
    for (orig, rest) in trail.entries().iter().zip(restored.iter()) {
        assert_eq!(orig.replay_run_id, rest.replay_run_id);
        assert_eq!(orig.hash(), rest.hash());
    }
}

// ── I-24: Full pipeline: merge → schedule → provenance → audit ───────────

#[test]
fn i24_full_pipeline_end_to_end() {
    // 1. Merge events from 3 panes
    let mut resolver = PaneMergeResolver::with_defaults();
    for pane in 1..=3u64 {
        let events: Vec<MergeEvent> = (0..3)
            .map(|seq| make_merge_event(1000 * (pane + seq), pane, seq, "ingress_text"))
            .collect();
        resolver.add_pane_stream(pane, events);
    }

    let merged = resolver.merge();
    assert_eq!(merged.len(), 9);

    // 2. Create RecorderEvents from merge order
    let recorder_events: Vec<RecorderEvent> = merged
        .iter()
        .enumerate()
        .map(|(i, m)| {
            make_event(
                m.source_pane_id,
                i as u64,
                m.merge_key.recorded_at_ms,
                m.merge_key.recorded_at_ms,
                &format!("data-{}", i),
            )
        })
        .collect();

    // 3. Schedule replay with provenance tracking
    let emitter = ReplayProvenanceEmitter::with_defaults("run-pipeline".to_string());
    let mut scheduler = ReplayScheduler::new(recorder_events, ReplayConfig::instant()).unwrap();

    let barrier = ReplayBarrier::new();
    let mut decision_count = 0u64;

    while let Some(step) = scheduler.next_step() {
        decision_count += 1;

        // Record provenance
        emitter.record(ProvenanceRecordParams {
            event_id: step.merge_event_id.clone(),
            decision_type: DecisionType::MergeReorder,
            rule_id: format!("merge-{}", step.cursor),
            definition_hash: "pipeline-hash".to_string(),
            output_summary: format!(
                "pane={} ts={}",
                step.merge_pane_id, step.merge_recorded_at_ms
            ),
            wall_clock_ms: step.merge_recorded_at_ms,
            virtual_clock_ms: step.clock.occurred_at_ms,
            input_data: serde_json::json!({"cursor": step.cursor}),
            event_context: None,
        });

        // Verify side-effect barrier blocks in replay mode
        let request = make_effect_request(
            EffectType::SendKeys,
            step.merge_pane_id,
            &format!("replay-cmd-{}", step.cursor),
        );
        let outcome = barrier.process(&request);
        assert!(!outcome.executed);
    }

    assert_eq!(decision_count, 9);
    assert_eq!(emitter.len(), 9);

    // 4. Create audit trail entry
    let trail = ReplayAuditTrail::new();
    trail.append(AuditEntryParams {
        replay_run_id: "run-pipeline".to_string(),
        actor: "integration-test".to_string(),
        started_at_ms: 0,
        completed_at_ms: 10000,
        artifact_ref: "merged-9-events".to_string(),
        override_ref: None,
        decision_count,
        anomaly_count: resolver.stats().anomaly_count as u64,
    });

    let verification = trail.verify();
    assert!(verification.chain_intact);

    // 5. Verify provenance JSONL can be exported
    let jsonl = emitter.to_jsonl();
    assert!(!jsonl.is_empty());
    let restored = ReplayProvenanceEmitter::from_jsonl(&jsonl).unwrap();
    assert_eq!(restored.len(), 9);
}
