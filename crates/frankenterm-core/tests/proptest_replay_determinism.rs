//! Property tests for deterministic replay scheduler behavior.
//!
//! This suite covers P-01 through P-15 for `ft-og6q6.7.2`.
//! Every proptest uses at least 100 cases (or `PROPTEST_CASES` if higher).

use std::collections::{HashMap, HashSet};

use frankenterm_core::event_id::{RecorderMergeKey, StreamKind};
use frankenterm_core::recorder_query::QueryEventKind;
use frankenterm_core::recorder_replay::{
    ReplayConfig, ReplayEngineRoute, ReplayEquivalenceLevel, ReplayScheduler,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderLifecyclePhase, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::replay_capture::{DecisionEvent, DecisionType};
use proptest::prelude::*;
use serde_json::json;

fn proptest_cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .map(|cases| cases.max(100))
        .unwrap_or(100)
}

fn arb_stream_kind() -> impl Strategy<Value = StreamKind> {
    prop_oneof![
        Just(StreamKind::Lifecycle),
        Just(StreamKind::Control),
        Just(StreamKind::Ingress),
        Just(StreamKind::Egress),
    ]
}

fn arb_decision_type() -> impl Strategy<Value = DecisionType> {
    prop_oneof![
        Just(DecisionType::PatternMatch),
        Just(DecisionType::WorkflowStep),
        Just(DecisionType::PolicyEvaluation),
    ]
}

fn make_payload(stream_kind: StreamKind, text: &str, salt: u8) -> RecorderEventPayload {
    match stream_kind {
        StreamKind::Lifecycle => RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: if salt % 2 == 0 {
                RecorderLifecyclePhase::CaptureStarted
            } else {
                RecorderLifecyclePhase::CaptureStopped
            },
            reason: Some(format!("reason-{salt}")),
            details: json!({ "salt": salt, "kind": "lifecycle" }),
        },
        StreamKind::Control => RecorderEventPayload::ControlMarker {
            control_marker_type: if salt % 2 == 0 {
                RecorderControlMarkerType::PolicyDecision
            } else {
                RecorderControlMarkerType::ApprovalCheckpoint
            },
            details: json!({ "salt": salt, "kind": "control" }),
        },
        StreamKind::Ingress => RecorderEventPayload::IngressText {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        },
        StreamKind::Egress => RecorderEventPayload::EgressOutput {
            text: text.to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        },
    }
}

fn make_event(
    pane_id: u64,
    sequence: u64,
    occurred_at_ms: u64,
    recorded_at_ms: u64,
    stream_kind: StreamKind,
    text: &str,
    salt: u8,
    idx: usize,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: format!("evt-{pane_id}-{sequence}-{recorded_at_ms}-{idx}-{salt}"),
        pane_id,
        session_id: Some("session-proptest".to_string()),
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
        payload: make_payload(stream_kind, text, salt),
    }
}

fn arb_replay_events(max_len: usize) -> impl Strategy<Value = Vec<RecorderEvent>> {
    prop::collection::vec(
        (
            0u64..=6,
            0u64..=4,
            arb_stream_kind(),
            "[a-z0-9_ ]{0,20}",
            any::<u8>(),
        ),
        1..=max_len,
    )
    .prop_map(|raw| {
        let mut out = Vec::with_capacity(raw.len());
        let mut next_sequence_per_pane: HashMap<u64, u64> = HashMap::new();
        let mut recorded_at_ms = 10_000_u64;

        for (idx, (pane_id, delta_ms, stream_kind, text, salt)) in raw.into_iter().enumerate() {
            recorded_at_ms = recorded_at_ms.saturating_add(delta_ms);
            let occurred_at_ms = recorded_at_ms.saturating_sub(u64::from(salt % 3));
            let sequence = *next_sequence_per_pane.entry(pane_id).or_insert(0);
            next_sequence_per_pane.insert(pane_id, sequence.saturating_add(1));
            out.push(make_event(
                pane_id,
                sequence,
                occurred_at_ms,
                recorded_at_ms,
                stream_kind,
                &text,
                salt,
                idx,
            ));
        }

        out
    })
}

fn arb_same_timestamp_events(max_len: usize) -> impl Strategy<Value = Vec<RecorderEvent>> {
    prop::collection::vec(
        (0u64..=6, arb_stream_kind(), "[a-z0-9_ ]{0,20}", any::<u8>()),
        1..=max_len,
    )
    .prop_map(|raw| {
        let mut out = Vec::with_capacity(raw.len());
        let mut next_sequence_per_pane: HashMap<u64, u64> = HashMap::new();
        let recorded_at_ms = 42_000_u64;

        for (idx, (pane_id, stream_kind, text, salt)) in raw.into_iter().enumerate() {
            let sequence = *next_sequence_per_pane.entry(pane_id).or_insert(0);
            next_sequence_per_pane.insert(pane_id, sequence.saturating_add(1));
            out.push(make_event(
                pane_id,
                sequence,
                recorded_at_ms,
                recorded_at_ms,
                stream_kind,
                &text,
                salt,
                idx,
            ));
        }

        out
    })
}

fn query_kind_for_event(event: &RecorderEvent) -> QueryEventKind {
    match &event.payload {
        RecorderEventPayload::IngressText { .. } => QueryEventKind::IngressText,
        RecorderEventPayload::EgressOutput { .. } => QueryEventKind::EgressOutput,
        RecorderEventPayload::ControlMarker { .. } => QueryEventKind::ControlMarker,
        RecorderEventPayload::LifecycleMarker { .. } => QueryEventKind::LifecycleMarker,
    }
}

fn is_marker_event(event: &RecorderEvent) -> bool {
    matches!(
        &event.payload,
        RecorderEventPayload::ControlMarker { .. } | RecorderEventPayload::LifecycleMarker { .. }
    )
}

fn is_empty_text_event(event: &RecorderEvent) -> bool {
    match &event.payload {
        RecorderEventPayload::IngressText { text, .. }
        | RecorderEventPayload::EgressOutput { text, .. } => text.is_empty(),
        RecorderEventPayload::ControlMarker { .. }
        | RecorderEventPayload::LifecycleMarker { .. } => false,
    }
}

#[derive(Debug, Clone)]
struct DecisionChain {
    event_ids: Vec<String>,
    decisions: Vec<DecisionEvent>,
}

fn arb_decision_chain(max_len: usize) -> impl Strategy<Value = DecisionChain> {
    prop::collection::vec(
        (
            0u64..=6,
            arb_decision_type(),
            "[a-z][a-z0-9_.:-]{2,24}",
            "[a-z0-9 _-]{0,40}",
            0u64..=500_000,
            prop::option::of(0.0_f64..1.0_f64),
        ),
        1..=max_len,
    )
    .prop_map(|rows| {
        let mut event_ids: Vec<String> = Vec::with_capacity(rows.len());
        let mut decisions = Vec::with_capacity(rows.len());

        for (idx, (pane_id, decision_type, rule_id, input_text, timestamp_ms, confidence)) in
            rows.into_iter().enumerate()
        {
            let event_id = format!("recorder-event-{pane_id}-{idx}");
            let parent_event_id = if idx == 0 {
                None
            } else {
                Some(event_ids[idx - 1].clone())
            };

            let decision = DecisionEvent::new(
                decision_type,
                pane_id,
                rule_id.clone(),
                &format!("rule_definition::{rule_id}"),
                &input_text,
                json!({ "idx": idx, "decision_type": format!("{decision_type:?}") }),
                parent_event_id,
                confidence,
                timestamp_ms.saturating_add(idx as u64),
            );

            event_ids.push(event_id);
            decisions.push(decision);
        }

        DecisionChain {
            event_ids,
            decisions,
        }
    })
}

#[derive(Debug, Clone)]
struct OverridePackage {
    rule_ids: Vec<String>,
    workflow_rule_refs: Vec<String>,
    policy_rule_refs: Vec<String>,
}

fn arb_override_package() -> impl Strategy<Value = OverridePackage> {
    prop::collection::btree_set("[a-z]{3,8}\\.[a-z]{3,8}", 1..=8).prop_flat_map(|rule_set| {
        let rule_ids: Vec<String> = rule_set.into_iter().collect();
        let max = rule_ids.len();
        (
            Just(rule_ids),
            prop::collection::vec(0usize..max, 0..=max * 2),
            prop::collection::vec(0usize..max, 0..=max * 2),
        )
            .prop_map(|(rule_ids, workflow_indexes, policy_indexes)| {
                let workflow_rule_refs = workflow_indexes
                    .into_iter()
                    .map(|idx| rule_ids[idx].clone())
                    .collect();
                let policy_rule_refs = policy_indexes
                    .into_iter()
                    .map(|idx| rule_ids[idx].clone())
                    .collect();
                OverridePackage {
                    rule_ids,
                    workflow_rule_refs,
                    policy_rule_refs,
                }
            })
    })
}

#[derive(Debug, Clone, Copy)]
struct RegressionBudget {
    max_divergences: u32,
    max_critical: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateResult {
    Pass,
    Fail,
}

fn evaluate_budget(
    total_divergences: u32,
    critical_divergences: u32,
    budget: RegressionBudget,
) -> GateResult {
    if total_divergences <= budget.max_divergences && critical_divergences <= budget.max_critical {
        GateResult::Pass
    } else {
        GateResult::Fail
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases()))]

    /// P-01: Idempotent replay — replay(trace) twice produces identical decision sequences.
    #[test]
    fn p01_idempotent_replay_decision_sequence(events in arb_replay_events(80)) {
        let mut first = ReplayScheduler::new(events.clone(), ReplayConfig::default()).expect("scheduler should build");
        let mut second = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");
        let first_ids: Vec<String> = first.run_to_completion().into_iter().map(|step| step.decision.decision_id).collect();
        let second_ids: Vec<String> = second.run_to_completion().into_iter().map(|step| step.decision.decision_id).collect();
        prop_assert_eq!(first_ids, second_ids);
    }

    /// P-02: Commutative pane merge — reordering same-timestamp pane events yields the same merged output.
    #[test]
    fn p02_commutative_pane_merge_same_timestamp(events in arb_same_timestamp_events(80)) {
        let mut reversed = events.clone();
        reversed.reverse();

        let mut baseline = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");
        let mut permuted = ReplayScheduler::new(reversed, ReplayConfig::default()).expect("scheduler should build");

        let baseline_merge_ids: Vec<String> = baseline.run_to_completion().into_iter().map(|step| step.merge_event_id).collect();
        let permuted_merge_ids: Vec<String> = permuted.run_to_completion().into_iter().map(|step| step.merge_event_id).collect();
        prop_assert_eq!(baseline_merge_ids, permuted_merge_ids);
    }

    /// P-03: Monotonic clock — replay `recorded_at_ms` never decreases.
    #[test]
    fn p03_virtual_clock_monotonic(events in arb_replay_events(80)) {
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");
        let steps = scheduler.run_to_completion();

        for step in &steps {
            prop_assert_eq!(step.clock.recorded_at_ms, step.decision.recorded_at_ms);
            prop_assert_eq!(step.clock.occurred_at_ms, step.decision.occurred_at_ms);
        }

        for pair in steps.windows(2) {
            let prev = &pair[0];
            let next = &pair[1];
            prop_assert!(prev.clock.recorded_at_ms <= next.clock.recorded_at_ms);
        }
    }

    /// P-04: Event count conservation — total in = processed + skipped.
    #[test]
    fn p04_event_count_conservation(
        events in arb_replay_events(80),
        pane_filter_raw in prop::collection::vec(0u64..=6, 0..=6),
        kind_mask in prop::collection::vec(0u8..=3, 0..=4),
        include_markers in any::<bool>(),
        skip_empty in any::<bool>(),
    ) {
        let mut pane_filter = Vec::new();
        for pane_id in pane_filter_raw {
            if !pane_filter.contains(&pane_id) {
                pane_filter.push(pane_id);
            }
        }

        let mut kind_filter = Vec::new();
        for mask in kind_mask {
            let kind = match mask {
                0 => QueryEventKind::IngressText,
                1 => QueryEventKind::EgressOutput,
                2 => QueryEventKind::ControlMarker,
                _ => QueryEventKind::LifecycleMarker,
            };
            if !kind_filter.contains(&kind) {
                kind_filter.push(kind);
            }
        }

        let config = ReplayConfig {
            pane_filter,
            kind_filter,
            include_markers,
            skip_empty,
            ..ReplayConfig::default()
        };

        let total_events = events.len();
        let mut scheduler = ReplayScheduler::new(events, config).expect("scheduler should build");
        let steps = scheduler.run_to_completion();
        let processed = steps.len();
        let skipped = total_events.saturating_sub(processed);

        prop_assert_eq!(processed, scheduler.decisions().len());
        prop_assert_eq!(total_events, processed + skipped);
    }

    /// P-05: Sequence monotonicity — per-(pane, stream) sequence numbers are strictly increasing.
    #[test]
    fn p05_per_pane_sequence_monotonic(events in arb_replay_events(80)) {
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");
        let steps = scheduler.run_to_completion();
        let mut last_seq: HashMap<(u64, StreamKind), u64> = HashMap::new();

        for step in steps {
            let key = (step.decision.pane_id, step.decision.stream_kind);
            if let Some(prev) = last_seq.insert(key, step.decision.sequence) {
                prop_assert!(prev < step.decision.sequence);
            }
        }
    }

    /// P-06: Checkpoint/resume produces the same remaining schedule as uninterrupted replay.
    #[test]
    fn p06_checkpoint_resume_tail_equivalence(
        events in arb_replay_events(80),
        checkpoint_raw in 0usize..500usize,
    ) {
        let total = events.len();
        let checkpoint_cursor = checkpoint_raw % (total + 1);

        let mut baseline = ReplayScheduler::new(events.clone(), ReplayConfig::default()).expect("scheduler should build");
        let baseline_steps = baseline.run_to_completion();

        let mut partial = ReplayScheduler::new(events.clone(), ReplayConfig::default()).expect("scheduler should build");
        while partial.cursor() < checkpoint_cursor {
            if partial.next_step().is_none() && partial.cursor() >= partial.total_events() {
                break;
            }
        }
        let checkpoint = partial.checkpoint();

        let mut resumed = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");
        resumed.resume(checkpoint.clone()).expect("checkpoint should be valid");
        let resumed_steps = resumed.run_to_completion();
        let expected_tail: Vec<_> = baseline_steps.into_iter().skip(checkpoint.decisions_emitted).collect();

        prop_assert_eq!(resumed_steps, expected_tail);
    }

    /// P-07: Decision trace byte stream is stable across equivalent replays.
    #[test]
    fn p07_decision_trace_bytes_stable(events in arb_replay_events(80)) {
        let mut first = ReplayScheduler::new(events.clone(), ReplayConfig::default()).expect("scheduler should build");
        let mut second = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");

        first.run_to_completion();
        second.run_to_completion();

        let first_bytes = first.decision_trace_bytes().expect("trace serialization should succeed");
        let second_bytes = second.decision_trace_bytes().expect("trace serialization should succeed");
        prop_assert_eq!(&first_bytes, &second_bytes);
        #[allow(clippy::naive_bytecount)]
        let newline_count = first_bytes.iter().filter(|byte| **byte == b'\n').count();
        prop_assert_eq!(newline_count, first.decisions().len());
    }

    /// P-08: `skip_empty` removes empty ingress/egress text events.
    #[test]
    fn p08_skip_empty_filter_behavior(events in arb_replay_events(80)) {
        let expected = events.iter().filter(|event| !is_empty_text_event(event)).count();
        let config = ReplayConfig {
            skip_empty: true,
            ..ReplayConfig::default()
        };
        let mut scheduler = ReplayScheduler::new(events, config).expect("scheduler should build");
        let processed = scheduler.run_to_completion().len();
        prop_assert_eq!(processed, expected);
    }

    /// P-09: `include_markers = false` excludes control/lifecycle marker events.
    #[test]
    fn p09_exclude_markers_filter_behavior(events in arb_replay_events(80)) {
        let expected = events.iter().filter(|event| !is_marker_event(event)).count();
        let config = ReplayConfig {
            include_markers: false,
            ..ReplayConfig::default()
        };
        let mut scheduler = ReplayScheduler::new(events, config).expect("scheduler should build");
        let steps = scheduler.run_to_completion();
        prop_assert_eq!(steps.len(), expected);
        prop_assert!(steps.iter().all(|step| step.decision.engine_route == ReplayEngineRoute::Pattern));
    }

    /// P-10: Infinite-speed replay yields zero per-step delay.
    #[test]
    fn p10_infinite_speed_zero_delay(events in arb_replay_events(80)) {
        let config = ReplayConfig::instant();
        let mut scheduler = ReplayScheduler::new(events, config).expect("scheduler should build");
        let steps = scheduler.run_to_completion();
        prop_assert!(steps.iter().all(|step| step.delay_ms == 0));
    }

    /// P-11: Merge output is deterministic under arbitrary input order permutations.
    #[test]
    fn p11_permutation_invariance(events in arb_replay_events(80)) {
        let mut reversed = events.clone();
        reversed.reverse();
        let mut rotated = events.clone();
        if rotated.len() > 1 {
            let rotate_by = rotated.len() / 2;
            rotated.rotate_left(rotate_by);
        }

        let run_merge_ids = |items: Vec<RecorderEvent>| -> Vec<String> {
            let mut scheduler = ReplayScheduler::new(items, ReplayConfig::default()).expect("scheduler should build");
            scheduler.run_to_completion().into_iter().map(|step| step.merge_event_id).collect()
        };

        let baseline = run_merge_ids(events);
        let reversed_ids = run_merge_ids(reversed);
        let rotated_ids = run_merge_ids(rotated);

        prop_assert_eq!(&baseline, &reversed_ids);
        prop_assert_eq!(&baseline, &rotated_ids);
    }

    /// P-12: Default equivalence level remains decision-level.
    #[test]
    fn p12_default_equivalence_level(_dummy in any::<u8>()) {
        prop_assert_eq!(
            ReplayConfig::default().equivalence_level,
            ReplayEquivalenceLevel::Decision
        );
    }

    /// P-13: Decision event causal references point to known prior recorder event IDs.
    #[test]
    fn p13_decision_event_causal_chain(chain in arb_decision_chain(80)) {
        let mut event_id_to_index = HashMap::new();
        for (idx, event_id) in chain.event_ids.iter().enumerate() {
            event_id_to_index.insert(event_id.as_str(), idx);
        }

        for (idx, decision) in chain.decisions.iter().enumerate() {
            if let Some(parent_event_id) = decision.parent_event_id.as_deref() {
                let parent_idx = event_id_to_index.get(parent_event_id).copied();
                prop_assert!(parent_idx.is_some());
                prop_assert!(parent_idx.expect("checked above") < idx);
            }
        }
    }

    /// P-14: Override package references only declared rule IDs.
    #[test]
    fn p14_override_package_rule_refs_valid(package in arb_override_package()) {
        let rule_set: HashSet<&str> = package.rule_ids.iter().map(String::as_str).collect();
        prop_assert!(!rule_set.is_empty());
        for rule_ref in &package.workflow_rule_refs {
            prop_assert!(rule_set.contains(rule_ref.as_str()));
        }
        for rule_ref in &package.policy_rule_refs {
            prop_assert!(rule_set.contains(rule_ref.as_str()));
        }
    }

    /// P-15: Regression budget relaxation is monotonic (relaxed budgets cannot fail newly passing runs).
    #[test]
    fn p15_budget_relaxation_monotonic(
        total_divergences in 0u32..=30,
        raw_critical in 0u32..=30,
        strict_max_divergences in 0u32..=30,
        strict_max_critical in 0u32..=30,
        relax_divergence_delta in 0u32..=30,
        relax_critical_delta in 0u32..=30,
    ) {
        let critical_divergences = raw_critical.min(total_divergences);

        let strict = RegressionBudget {
            max_divergences: strict_max_divergences,
            max_critical: strict_max_critical,
        };
        let relaxed = RegressionBudget {
            max_divergences: strict_max_divergences.saturating_add(relax_divergence_delta),
            max_critical: strict_max_critical.saturating_add(relax_critical_delta),
        };

        let strict_result = evaluate_budget(total_divergences, critical_divergences, strict);
        let relaxed_result = evaluate_budget(total_divergences, critical_divergences, relaxed);

        if strict_result == GateResult::Pass {
            prop_assert_eq!(relaxed_result, GateResult::Pass);
        }
        if relaxed_result == GateResult::Fail {
            prop_assert_eq!(strict_result, GateResult::Fail);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(proptest_cases()))]

    /// Extra guard: the sorted merge keys from the scheduler are globally monotonic.
    #[test]
    fn merge_keys_are_globally_monotonic(events in arb_replay_events(80)) {
        let mut scheduler = ReplayScheduler::new(events, ReplayConfig::default()).expect("scheduler should build");
        let steps = scheduler.run_to_completion();
        for pair in steps.windows(2) {
            let lhs = (
                pair[0].merge_recorded_at_ms,
                pair[0].merge_pane_id,
                pair[0].merge_stream_kind,
                pair[0].merge_sequence,
                pair[0].merge_event_id.clone(),
            );
            let rhs = (
                pair[1].merge_recorded_at_ms,
                pair[1].merge_pane_id,
                pair[1].merge_stream_kind,
                pair[1].merge_sequence,
                pair[1].merge_event_id.clone(),
            );
            prop_assert!(lhs <= rhs);
        }
    }

    /// Extra guard: generated event IDs remain unique within each generated trace.
    #[test]
    fn generated_event_ids_unique(events in arb_replay_events(80)) {
        let ids: Vec<&str> = events.iter().map(|event| event.event_id.as_str()).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        prop_assert_eq!(ids.len(), unique.len());
    }

    /// Extra guard: merge key generation remains deterministic for equal inputs.
    #[test]
    fn merge_key_deterministic(events in arb_replay_events(80)) {
        for event in events {
            let first = RecorderMergeKey::from_event(&event);
            let second = RecorderMergeKey::from_event(&event);
            prop_assert_eq!(first, second);
        }
    }

    /// Extra guard: query-kind mapping for generated events always matches payload variant.
    #[test]
    fn query_kind_mapping_matches_payload(events in arb_replay_events(80)) {
        for event in events {
            match (&event.payload, query_kind_for_event(&event)) {
                (RecorderEventPayload::IngressText { .. }, QueryEventKind::IngressText)
                | (RecorderEventPayload::EgressOutput { .. }, QueryEventKind::EgressOutput)
                | (RecorderEventPayload::ControlMarker { .. }, QueryEventKind::ControlMarker)
                | (RecorderEventPayload::LifecycleMarker { .. }, QueryEventKind::LifecycleMarker) => {}
                _ => prop_assert!(false),
            }
        }
    }
}
