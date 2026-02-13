//! Property-based tests for event_id module.
//!
//! Verifies deterministic hashing, merge ordering, and clock anomaly invariants:
//! - generate_event_id_v1 is deterministic (same input → same output)
//! - generate_event_id_v1 output is always 64 hex chars (SHA-256)
//! - Distinct events (differing in any field) produce distinct IDs
//! - StreamKind ordering: total, antisymmetric, transitive, consistent with rank()
//! - RecorderMergeKey: total ordering with correct 5-level tiebreak precedence
//! - RecorderMergeKey: sorting is deterministic (stable across runs)
//! - Clock anomaly: regression always detected, forward within threshold is OK
//! - ClockAnomalyTracker: per-domain isolation (pane × stream)
//! - ClockAnomalyTracker: baseline updates after anomaly (recovery)
//! - Merge key from_event roundtrip preserves ordering fields
//!
//! Bead: wa-3nx8

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::event_id::{
    ClockAnomalyTracker, RecorderMergeKey, StreamKind, detect_clock_anomaly,
    generate_event_id_v1,
};
use frankenterm_core::recording::{
    RecorderControlMarkerType, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderLifecyclePhase, RecorderRedactionLevel,
    RecorderSegmentKind, RecorderTextEncoding, RECORDER_EVENT_SCHEMA_VERSION_V1,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_stream_kind() -> impl Strategy<Value = StreamKind> {
    prop_oneof![
        Just(StreamKind::Lifecycle),
        Just(StreamKind::Control),
        Just(StreamKind::Ingress),
        Just(StreamKind::Egress),
    ]
}

fn arb_event_source() -> impl Strategy<Value = RecorderEventSource> {
    prop_oneof![
        Just(RecorderEventSource::WeztermMux),
        Just(RecorderEventSource::RobotMode),
        Just(RecorderEventSource::WorkflowEngine),
        Just(RecorderEventSource::OperatorAction),
        Just(RecorderEventSource::RecoveryFlow),
    ]
}

fn arb_text_encoding() -> impl Strategy<Value = RecorderTextEncoding> {
    Just(RecorderTextEncoding::Utf8)
}

fn arb_redaction_level() -> impl Strategy<Value = RecorderRedactionLevel> {
    prop_oneof![
        Just(RecorderRedactionLevel::None),
        Just(RecorderRedactionLevel::Partial),
        Just(RecorderRedactionLevel::Full),
    ]
}

fn arb_ingress_kind() -> impl Strategy<Value = RecorderIngressKind> {
    prop_oneof![
        Just(RecorderIngressKind::SendText),
        Just(RecorderIngressKind::Paste),
        Just(RecorderIngressKind::WorkflowAction),
    ]
}

fn arb_segment_kind() -> impl Strategy<Value = RecorderSegmentKind> {
    prop_oneof![
        Just(RecorderSegmentKind::Delta),
        Just(RecorderSegmentKind::Gap),
        Just(RecorderSegmentKind::Snapshot),
    ]
}

fn arb_control_marker_type() -> impl Strategy<Value = RecorderControlMarkerType> {
    prop_oneof![
        Just(RecorderControlMarkerType::PromptBoundary),
        Just(RecorderControlMarkerType::Resize),
        Just(RecorderControlMarkerType::PolicyDecision),
        Just(RecorderControlMarkerType::ApprovalCheckpoint),
    ]
}

fn arb_lifecycle_phase() -> impl Strategy<Value = RecorderLifecyclePhase> {
    prop_oneof![
        Just(RecorderLifecyclePhase::CaptureStarted),
        Just(RecorderLifecyclePhase::CaptureStopped),
        Just(RecorderLifecyclePhase::PaneOpened),
        Just(RecorderLifecyclePhase::PaneClosed),
        Just(RecorderLifecyclePhase::ReplayStarted),
        Just(RecorderLifecyclePhase::ReplayFinished),
    ]
}

fn arb_payload() -> impl Strategy<Value = RecorderEventPayload> {
    prop_oneof![
        // IngressText
        (
            ".*",
            arb_text_encoding(),
            arb_redaction_level(),
            arb_ingress_kind(),
        )
            .prop_map(|(text, encoding, redaction, ingress_kind)| {
                RecorderEventPayload::IngressText {
                    text,
                    encoding,
                    redaction,
                    ingress_kind,
                }
            }),
        // EgressOutput
        (
            ".*",
            arb_text_encoding(),
            arb_redaction_level(),
            arb_segment_kind(),
            any::<bool>(),
        )
            .prop_map(
                |(text, encoding, redaction, segment_kind, is_gap)| {
                    RecorderEventPayload::EgressOutput {
                        text,
                        encoding,
                        redaction,
                        segment_kind,
                        is_gap,
                    }
                }
            ),
        // ControlMarker
        arb_control_marker_type().prop_map(|control_marker_type| {
            RecorderEventPayload::ControlMarker {
                control_marker_type,
                details: serde_json::json!({}),
            }
        }),
        // LifecycleMarker
        (arb_lifecycle_phase(), proptest::option::of(".*")).prop_map(
            |(lifecycle_phase, reason)| {
                RecorderEventPayload::LifecycleMarker {
                    lifecycle_phase,
                    reason,
                    details: serde_json::json!({}),
                }
            },
        ),
    ]
}

fn arb_causality() -> impl Strategy<Value = RecorderEventCausality> {
    (
        proptest::option::of("[a-f0-9]{16}"),
        proptest::option::of("[a-f0-9]{16}"),
        proptest::option::of("[a-f0-9]{16}"),
    )
        .prop_map(
            |(parent_event_id, trigger_event_id, root_event_id)| RecorderEventCausality {
                parent_event_id,
                trigger_event_id,
                root_event_id,
            },
        )
}

fn arb_recorder_event() -> impl Strategy<Value = RecorderEvent> {
    (
        0u64..=100,          // pane_id
        0u64..=1000,         // sequence
        1u64..=1_000_000,    // occurred_at_ms
        1u64..=1_000_000,    // recorded_at_ms
        arb_event_source(),
        arb_payload(),
        arb_causality(),
        proptest::option::of("[a-z0-9]{8}"), // session_id
        proptest::option::of("[a-z0-9]{8}"), // workflow_id
        proptest::option::of("[a-z0-9]{8}"), // correlation_id
    )
        .prop_map(
            |(
                pane_id,
                sequence,
                occurred_at_ms,
                recorded_at_ms,
                source,
                payload,
                causality,
                session_id,
                workflow_id,
                correlation_id,
            )| {
                RecorderEvent {
                    schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                    event_id: String::new(),
                    pane_id,
                    session_id,
                    workflow_id,
                    correlation_id,
                    source,
                    occurred_at_ms,
                    recorded_at_ms,
                    sequence,
                    causality,
                    payload,
                }
            },
        )
}

fn arb_merge_key() -> impl Strategy<Value = RecorderMergeKey> {
    (
        0u64..=1_000_000,    // recorded_at_ms
        0u64..=100,          // pane_id
        arb_stream_kind(),
        0u64..=1000,         // sequence
        "[a-f0-9]{8}",       // event_id
    )
        .prop_map(
            |(recorded_at_ms, pane_id, stream_kind, sequence, event_id)| RecorderMergeKey {
                recorded_at_ms,
                pane_id,
                stream_kind,
                sequence,
                event_id,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// Event ID determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// The same event always produces the same event ID.
    #[test]
    fn prop_event_id_deterministic(event in arb_recorder_event()) {
        let id1 = generate_event_id_v1(&event);
        let id2 = generate_event_id_v1(&event);
        prop_assert_eq!(&id1, &id2, "event ID must be deterministic");
    }

    /// Event IDs are always 64 hex characters (SHA-256 hex digest).
    #[test]
    fn prop_event_id_format(event in arb_recorder_event()) {
        let id = generate_event_id_v1(&event);
        prop_assert_eq!(id.len(), 64, "SHA-256 hex digest must be 64 chars");
        prop_assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "event ID must contain only hex digits, got: {}",
            id
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Event ID collision resistance: differing in any single field
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Events differing only in pane_id produce different IDs.
    #[test]
    fn prop_event_id_differs_by_pane(
        event in arb_recorder_event(),
        other_pane in 101u64..=200,
    ) {
        let mut e2 = event.clone();
        e2.pane_id = if event.pane_id == other_pane {
            other_pane + 1
        } else {
            other_pane
        };
        let id1 = generate_event_id_v1(&event);
        let id2 = generate_event_id_v1(&e2);
        prop_assert_ne!(id1, id2, "different pane_id must produce different IDs");
    }

    /// Events differing only in sequence produce different IDs.
    #[test]
    fn prop_event_id_differs_by_sequence(
        event in arb_recorder_event(),
        delta in 1u64..=500,
    ) {
        let mut e2 = event.clone();
        e2.sequence = event.sequence.wrapping_add(delta);
        if e2.sequence == event.sequence {
            e2.sequence = event.sequence.wrapping_add(1);
        }
        let id1 = generate_event_id_v1(&event);
        let id2 = generate_event_id_v1(&e2);
        prop_assert_ne!(id1, id2, "different sequence must produce different IDs");
    }

    /// Events differing only in occurred_at_ms produce different IDs.
    #[test]
    fn prop_event_id_differs_by_timestamp(
        event in arb_recorder_event(),
        delta in 1u64..=500,
    ) {
        let mut e2 = event.clone();
        e2.occurred_at_ms = event.occurred_at_ms.wrapping_add(delta);
        if e2.occurred_at_ms == event.occurred_at_ms {
            e2.occurred_at_ms = event.occurred_at_ms.wrapping_add(1);
        }
        let id1 = generate_event_id_v1(&event);
        let id2 = generate_event_id_v1(&e2);
        prop_assert_ne!(id1, id2, "different timestamp must produce different IDs");
    }

    /// Events differing only in text content produce different IDs.
    #[test]
    fn prop_event_id_differs_by_text(
        pane_id in 0u64..=50,
        seq in 0u64..=100,
        ts in 1u64..=1_000_000,
        text1 in ".{1,50}",
        text2 in ".{1,50}",
    ) {
        prop_assume!(text1 != text2);
        let e1 = make_ingress_event(pane_id, seq, ts, &text1);
        let e2 = make_ingress_event(pane_id, seq, ts, &text2);
        let id1 = generate_event_id_v1(&e1);
        let id2 = generate_event_id_v1(&e2);
        prop_assert_ne!(id1, id2, "different text must produce different IDs");
    }

    /// Batch of events with unique (pane_id, seq, ts, payload) tuples have unique IDs.
    #[test]
    fn prop_event_id_batch_uniqueness(
        events in prop::collection::vec(arb_recorder_event(), 2..=20),
    ) {
        let ids: Vec<String> = events.iter().map(generate_event_id_v1).collect();
        let unique: HashSet<&String> = ids.iter().collect();
        // With random fields, collisions in < 20 events are astronomically unlikely
        // but not impossible. We check format is always valid.
        for id in &ids {
            prop_assert_eq!(id.len(), 64, "all IDs must be 64 hex chars");
        }
        // If any two events are bitwise identical, they should produce the same ID.
        // If all events differ, all IDs should differ.
        let event_set: HashSet<String> = events
            .iter()
            .map(|e| format!("{:?}", e))
            .collect();
        if event_set.len() == events.len() {
            prop_assert_eq!(
                unique.len(),
                ids.len(),
                "distinct events must produce distinct IDs"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// StreamKind ordering invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// StreamKind Ord is consistent with rank().
    #[test]
    fn prop_stream_kind_ord_consistent_with_rank(
        a in arb_stream_kind(),
        b in arb_stream_kind(),
    ) {
        let ord_by_rank = a.rank().cmp(&b.rank());
        let ord_by_trait = a.cmp(&b);
        prop_assert_eq!(
            ord_by_rank, ord_by_trait,
            "Ord must agree with rank(): a={:?} b={:?}", a, b
        );
    }

    /// StreamKind ordering is antisymmetric: if a < b then b > a.
    #[test]
    fn prop_stream_kind_antisymmetric(
        a in arb_stream_kind(),
        b in arb_stream_kind(),
    ) {
        let ab = a.cmp(&b);
        let ba = b.cmp(&a);
        prop_assert_eq!(
            ab, ba.reverse(),
            "ordering must be antisymmetric: a={:?} b={:?}", a, b
        );
    }

    /// StreamKind ordering is transitive: if a <= b and b <= c then a <= c.
    #[test]
    fn prop_stream_kind_transitive(
        a in arb_stream_kind(),
        b in arb_stream_kind(),
        c in arb_stream_kind(),
    ) {
        use std::cmp::Ordering;
        if a.cmp(&b) != Ordering::Greater && b.cmp(&c) != Ordering::Greater {
            prop_assert!(
                a.cmp(&c) != Ordering::Greater,
                "transitivity violated: {:?} <= {:?} <= {:?} but {:?} > {:?}",
                a, b, c, a, c
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// RecorderMergeKey: total ordering invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// MergeKey ordering is reflexive: a == a.
    #[test]
    fn prop_merge_key_reflexive(key in arb_merge_key()) {
        prop_assert_eq!(
            key.cmp(&key),
            std::cmp::Ordering::Equal,
            "key must equal itself"
        );
    }

    /// MergeKey ordering is antisymmetric.
    #[test]
    fn prop_merge_key_antisymmetric(
        a in arb_merge_key(),
        b in arb_merge_key(),
    ) {
        let ab = a.cmp(&b);
        let ba = b.cmp(&a);
        prop_assert_eq!(
            ab, ba.reverse(),
            "antisymmetry violated for merge keys"
        );
    }

    /// MergeKey ordering is transitive.
    #[test]
    fn prop_merge_key_transitive(
        a in arb_merge_key(),
        b in arb_merge_key(),
        c in arb_merge_key(),
    ) {
        use std::cmp::Ordering;
        if a.cmp(&b) != Ordering::Greater && b.cmp(&c) != Ordering::Greater {
            prop_assert!(
                a.cmp(&c) != Ordering::Greater,
                "transitivity violated for merge keys"
            );
        }
    }

    /// Sorting a vec of merge keys is deterministic (idempotent).
    #[test]
    fn prop_merge_key_sort_deterministic(
        keys in prop::collection::vec(arb_merge_key(), 2..=30),
    ) {
        let mut sorted1 = keys.clone();
        let mut sorted2 = keys;
        sorted1.sort();
        sorted2.sort();
        prop_assert_eq!(sorted1, sorted2, "sort must be deterministic");
    }

    /// MergeKey primary sort is by recorded_at_ms.
    #[test]
    fn prop_merge_key_primary_sort_timestamp(
        ts1 in 0u64..=500_000,
        ts2 in 500_001u64..=1_000_000,
        pane_id in 0u64..=10,
        stream in arb_stream_kind(),
        seq in 0u64..=100,
        eid in "[a-f0-9]{8}",
    ) {
        let k1 = RecorderMergeKey {
            recorded_at_ms: ts1,
            pane_id,
            stream_kind: stream,
            sequence: seq,
            event_id: eid.clone(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: ts2,
            pane_id,
            stream_kind: stream,
            sequence: seq,
            event_id: eid,
        };
        prop_assert!(
            k1 < k2,
            "lower timestamp must sort first: {} < {}", ts1, ts2
        );
    }

    /// MergeKey tiebreak: pane_id when timestamps are equal.
    #[test]
    fn prop_merge_key_tiebreak_pane(
        ts in 0u64..=1_000_000,
        p1 in 0u64..=49,
        p2 in 50u64..=100,
        stream in arb_stream_kind(),
        seq in 0u64..=100,
        eid in "[a-f0-9]{8}",
    ) {
        let k1 = RecorderMergeKey {
            recorded_at_ms: ts,
            pane_id: p1,
            stream_kind: stream,
            sequence: seq,
            event_id: eid.clone(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: ts,
            pane_id: p2,
            stream_kind: stream,
            sequence: seq,
            event_id: eid,
        };
        prop_assert!(
            k1 < k2,
            "lower pane_id must sort first when ts equal: {} < {}", p1, p2
        );
    }

    /// MergeKey tiebreak: stream_kind rank when ts and pane equal.
    #[test]
    fn prop_merge_key_tiebreak_stream(
        ts in 0u64..=1_000_000,
        pane_id in 0u64..=100,
        seq in 0u64..=100,
        eid in "[a-f0-9]{8}",
    ) {
        let k_lifecycle = RecorderMergeKey {
            recorded_at_ms: ts,
            pane_id,
            stream_kind: StreamKind::Lifecycle,
            sequence: seq,
            event_id: eid.clone(),
        };
        let k_egress = RecorderMergeKey {
            recorded_at_ms: ts,
            pane_id,
            stream_kind: StreamKind::Egress,
            sequence: seq,
            event_id: eid,
        };
        prop_assert!(
            k_lifecycle < k_egress,
            "lifecycle (rank 0) must sort before egress (rank 3)"
        );
    }

    /// MergeKey tiebreak: sequence when ts, pane, stream all equal.
    #[test]
    fn prop_merge_key_tiebreak_sequence(
        ts in 0u64..=1_000_000,
        pane_id in 0u64..=100,
        stream in arb_stream_kind(),
        s1 in 0u64..=499,
        s2 in 500u64..=1000,
        eid in "[a-f0-9]{8}",
    ) {
        let k1 = RecorderMergeKey {
            recorded_at_ms: ts,
            pane_id,
            stream_kind: stream,
            sequence: s1,
            event_id: eid.clone(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: ts,
            pane_id,
            stream_kind: stream,
            sequence: s2,
            event_id: eid,
        };
        prop_assert!(
            k1 < k2,
            "lower sequence must sort first: {} < {}", s1, s2
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// MergeKey from_event consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// MergeKey::from_event preserves ordering-relevant fields.
    #[test]
    fn prop_merge_key_from_event_fields(event in arb_recorder_event()) {
        let key = RecorderMergeKey::from_event(&event);
        prop_assert_eq!(key.recorded_at_ms, event.recorded_at_ms);
        prop_assert_eq!(key.pane_id, event.pane_id);
        prop_assert_eq!(key.stream_kind, StreamKind::from_payload(&event.payload));
        prop_assert_eq!(key.sequence, event.sequence);
    }

    /// Two events with identical merge-relevant fields produce equal merge keys.
    #[test]
    fn prop_merge_key_equality_from_events(
        pane_id in 0u64..=10,
        seq in 0u64..=100,
        ts in 1u64..=1_000_000,
        text in ".{1,20}",
    ) {
        let e1 = make_ingress_event(pane_id, seq, ts, &text);
        let mut e2 = e1.clone();
        // event_id differs but merge key uses it — set them equal
        e2.event_id = e1.event_id.clone();
        let k1 = RecorderMergeKey::from_event(&e1);
        let k2 = RecorderMergeKey::from_event(&e2);
        prop_assert_eq!(k1, k2, "identical events must produce equal merge keys");
    }
}

// ────────────────────────────────────────────────────────────────────
// Clock anomaly detection
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Forward progress (current >= prev) with no threshold is never anomalous.
    #[test]
    fn prop_clock_no_anomaly_forward(
        prev in 0u64..=500_000,
        delta in 0u64..=500_000,
    ) {
        let current = prev.saturating_add(delta);
        let result = detect_clock_anomaly(current, prev, 0);
        prop_assert!(
            !result.is_anomaly,
            "forward progress must not be anomalous: current={}, prev={}",
            current, prev
        );
    }

    /// Backwards clock (current < prev) is always anomalous.
    #[test]
    fn prop_clock_regression_always_detected(
        prev in 1u64..=1_000_000,
        regression in 1u64..=1_000_000,
    ) {
        let current = prev.saturating_sub(regression);
        prop_assume!(current < prev);
        let result = detect_clock_anomaly(current, prev, 0);
        prop_assert!(
            result.is_anomaly,
            "regression must be detected: current={}, prev={}",
            current, prev
        );
        prop_assert!(
            result.reason.as_ref().unwrap().contains("regression"),
            "reason must mention regression"
        );
    }

    /// Future skew detected when delta exceeds threshold.
    #[test]
    fn prop_clock_future_skew_detected(
        prev in 0u64..=100_000,
        threshold in 1u64..=10_000,
        excess in 1u64..=100_000,
    ) {
        let current = prev.saturating_add(threshold).saturating_add(excess);
        prop_assume!(current > prev + threshold);
        let result = detect_clock_anomaly(current, prev, threshold);
        prop_assert!(
            result.is_anomaly,
            "future skew must be detected: current={}, prev={}, threshold={}",
            current, prev, threshold
        );
        prop_assert!(
            result.reason.as_ref().unwrap().contains("future skew"),
            "reason must mention future skew"
        );
    }

    /// Within-threshold forward delta is not anomalous.
    #[test]
    fn prop_clock_within_threshold_ok(
        prev in 0u64..=500_000,
        threshold in 1u64..=500_000,
        delta in 0u64..=500_000,
    ) {
        let current = prev.saturating_add(delta.min(threshold));
        prop_assume!(current >= prev && current <= prev + threshold);
        let result = detect_clock_anomaly(current, prev, threshold);
        prop_assert!(
            !result.is_anomaly,
            "within-threshold delta must not be anomalous: current={}, prev={}, threshold={}",
            current, prev, threshold
        );
    }

    /// Zero threshold disables future-skew detection entirely.
    #[test]
    fn prop_clock_zero_threshold_no_future_skew(
        prev in 0u64..=100_000,
        delta in 0u64..=u32::MAX as u64,
    ) {
        let current = prev.saturating_add(delta);
        let result = detect_clock_anomaly(current, prev, 0);
        prop_assert!(
            !result.is_anomaly,
            "zero threshold must never trigger future skew"
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// ClockAnomalyTracker: per-domain isolation
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// First observation for any domain is never anomalous.
    #[test]
    fn prop_tracker_first_observation_ok(
        pane_id in 0u64..=100,
        stream in arb_stream_kind(),
        ts in 0u64..=1_000_000,
    ) {
        let mut tracker = ClockAnomalyTracker::new(0);
        let result = tracker.observe(pane_id, stream, ts);
        prop_assert!(!result.is_anomaly, "first observation must not be anomalous");
        prop_assert_eq!(tracker.domain_count(), 1);
    }

    /// Different pane IDs are independent domains — regression in one doesn't affect another.
    #[test]
    fn prop_tracker_pane_isolation(
        p1 in 0u64..=49,
        p2 in 50u64..=100,
        high_ts in 500u64..=1_000_000,
        low_ts in 0u64..=499,
    ) {
        let mut tracker = ClockAnomalyTracker::new(0);
        // Pane 1 observes high timestamp
        tracker.observe(p1, StreamKind::Ingress, high_ts);
        // Pane 2 observes low timestamp — not a regression because different domain
        let result = tracker.observe(p2, StreamKind::Ingress, low_ts);
        prop_assert!(
            !result.is_anomaly,
            "different panes must be independent: p1={} p2={}", p1, p2
        );
    }

    /// Different stream kinds on the same pane are independent domains.
    #[test]
    fn prop_tracker_stream_isolation(
        pane_id in 0u64..=100,
        high_ts in 500u64..=1_000_000,
        low_ts in 0u64..=499,
    ) {
        let mut tracker = ClockAnomalyTracker::new(0);
        tracker.observe(pane_id, StreamKind::Ingress, high_ts);
        let result = tracker.observe(pane_id, StreamKind::Egress, low_ts);
        prop_assert!(
            !result.is_anomaly,
            "different streams must be independent"
        );
    }

    /// After an anomaly, the baseline updates so subsequent forward progress is OK.
    #[test]
    fn prop_tracker_recovery_after_anomaly(
        pane_id in 0u64..=50,
        stream in arb_stream_kind(),
        high_ts in 500u64..=1_000_000,
        low_ts in 0u64..=499,
        recovery_delta in 1u64..=1000,
    ) {
        let mut tracker = ClockAnomalyTracker::new(0);
        tracker.observe(pane_id, stream, high_ts);
        // This should be an anomaly (regression)
        let anomaly = tracker.observe(pane_id, stream, low_ts);
        prop_assert!(anomaly.is_anomaly, "regression expected");
        // Recovery: forward from the new baseline
        let recovery_ts = low_ts + recovery_delta;
        let result = tracker.observe(pane_id, stream, recovery_ts);
        prop_assert!(
            !result.is_anomaly,
            "recovery after anomaly must work: low_ts={}, recovery_ts={}",
            low_ts, recovery_ts
        );
    }

    /// Domain count grows as distinct (pane, stream) pairs are observed.
    #[test]
    fn prop_tracker_domain_count(
        observations in prop::collection::vec(
            (0u64..=10, arb_stream_kind(), 0u64..=1_000_000),
            1..=30,
        ),
    ) {
        let mut tracker = ClockAnomalyTracker::new(0);
        let mut seen = HashSet::new();
        for &(pane_id, stream, ts) in &observations {
            tracker.observe(pane_id, stream, ts);
            seen.insert((pane_id, stream));
        }
        prop_assert_eq!(
            tracker.domain_count(),
            seen.len(),
            "domain count must equal number of distinct (pane, stream) pairs"
        );
    }

    /// Monotonic sequence on the same domain is never anomalous (with no threshold).
    #[test]
    fn prop_tracker_monotonic_sequence_ok(
        pane_id in 0u64..=50,
        stream in arb_stream_kind(),
        start_ts in 0u64..=100_000,
        deltas in prop::collection::vec(0u64..=1000, 1..=20),
    ) {
        let mut tracker = ClockAnomalyTracker::new(0);
        let mut ts = start_ts;
        for delta in deltas {
            let result = tracker.observe(pane_id, stream, ts);
            prop_assert!(
                !result.is_anomaly,
                "monotonic sequence must not be anomalous at ts={}",
                ts
            );
            ts = ts.saturating_add(delta);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// StreamKind from_payload consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// StreamKind::from_payload correctly classifies all payload variants.
    #[test]
    fn prop_stream_kind_from_payload(payload in arb_payload()) {
        let kind = StreamKind::from_payload(&payload);
        match &payload {
            RecorderEventPayload::IngressText { .. } => {
                prop_assert_eq!(kind, StreamKind::Ingress);
            }
            RecorderEventPayload::EgressOutput { .. } => {
                prop_assert_eq!(kind, StreamKind::Egress);
            }
            RecorderEventPayload::ControlMarker { .. } => {
                prop_assert_eq!(kind, StreamKind::Control);
            }
            RecorderEventPayload::LifecycleMarker { .. } => {
                prop_assert_eq!(kind, StreamKind::Lifecycle);
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// StreamKind serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// StreamKind survives JSON serialization roundtrip.
    #[test]
    fn prop_stream_kind_serde_roundtrip(kind in arb_stream_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: StreamKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back, "serde roundtrip must preserve StreamKind");
    }
}

// ────────────────────────────────────────────────────────────────────
// Helper
// ────────────────────────────────────────────────────────────────────

fn make_ingress_event(pane_id: u64, seq: u64, ts: u64, text: &str) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: String::new(),
        pane_id,
        session_id: Some("test-session".into()),
        workflow_id: None,
        correlation_id: None,
        source: RecorderEventSource::RobotMode,
        occurred_at_ms: ts,
        recorded_at_ms: ts + 1,
        sequence: seq,
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
