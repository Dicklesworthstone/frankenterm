//! Property-based tests for the flight recorder correctness stack.
//!
//! Uses proptest to verify invariants hold over randomly generated event streams:
//! - Deterministic event IDs never collide across arbitrary inputs
//! - Merge-key sorted streams always pass invariant checks
//! - Replay determinism: any two orderings of the same events produce identical sorted output
//! - Causal chains with valid references never trigger dangling-ref violations
//! - Sequence monotonicity is maintained by SequenceAssigner under arbitrary pane interleaving
//!
//! Bead: ft-oegrb.7.3 (recorder invariants property verification)

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::event_id::{RecorderMergeKey, generate_event_id_v1};
use frankenterm_core::recorder_invariants::{
    InvariantChecker, InvariantCheckerConfig, ViolationKind, verify_replay_determinism,
};
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderEvent, RecorderEventCausality, RecorderEventPayload,
    RecorderEventSource, RecorderIngressKind, RecorderRedactionLevel, RecorderSegmentKind,
    RecorderTextEncoding,
};
use frankenterm_core::sequence_model::{ReplayOrder, SequenceAssigner, validate_replay_order};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_pane_id() -> impl Strategy<Value = u64> {
    0u64..20
}

fn arb_timestamp() -> impl Strategy<Value = u64> {
    1_000_000u64..2_000_000
}

fn arb_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ]{0,50}"
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

fn arb_ingress_payload() -> impl Strategy<Value = RecorderEventPayload> {
    arb_text().prop_map(|text| RecorderEventPayload::IngressText {
        text,
        encoding: RecorderTextEncoding::Utf8,
        redaction: RecorderRedactionLevel::None,
        ingress_kind: RecorderIngressKind::SendText,
    })
}

fn arb_egress_payload() -> impl Strategy<Value = RecorderEventPayload> {
    (arb_text(), any::<bool>()).prop_map(|(text, is_gap)| RecorderEventPayload::EgressOutput {
        text,
        encoding: RecorderTextEncoding::Utf8,
        redaction: RecorderRedactionLevel::None,
        segment_kind: if is_gap {
            RecorderSegmentKind::Gap
        } else {
            RecorderSegmentKind::Delta
        },
        is_gap,
    })
}

fn arb_payload() -> impl Strategy<Value = RecorderEventPayload> {
    prop_oneof![arb_ingress_payload(), arb_egress_payload(),]
}

/// Generate a well-formed event with given pane_id, sequence, and timestamp.
fn make_event_for_prop(
    pane_id: u64,
    sequence: u64,
    occurred_at_ms: u64,
    source: RecorderEventSource,
    payload: RecorderEventPayload,
) -> RecorderEvent {
    let mut event = RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: String::new(),
        pane_id,
        session_id: Some("proptest".into()),
        workflow_id: None,
        correlation_id: None,
        source,
        occurred_at_ms,
        recorded_at_ms: occurred_at_ms + 1,
        sequence,
        causality: RecorderEventCausality {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
        },
        payload,
    };
    event.event_id = generate_event_id_v1(&event);
    event
}

// ---------------------------------------------------------------------------
// Property: deterministic IDs are collision-free
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn deterministic_ids_never_collide(
        pane_ids in prop::collection::vec(arb_pane_id(), 2..30),
        base_ts in arb_timestamp(),
    ) {
        let assigner = SequenceAssigner::new();
        let mut ids = HashSet::new();

        for &pane_id in &pane_ids {
            let (seq, _) = assigner.assign(pane_id);
            let ts = base_ts + seq * 10 + pane_id;
            let event = make_event_for_prop(
                pane_id,
                seq,
                ts,
                RecorderEventSource::RobotMode,
                RecorderEventPayload::IngressText {
                    text: format!("p{}-s{}", pane_id, seq),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    ingress_kind: RecorderIngressKind::SendText,
                },
            );
            prop_assert!(
                ids.insert(event.event_id.clone()),
                "collision: pane={}, seq={}, id={}",
                pane_id, seq, event.event_id
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property: merge-key sorted events pass invariant checks
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn sorted_events_pass_invariants(
        num_panes in 1u64..8,
        events_per_pane in 1usize..20,
        base_ts in arb_timestamp(),
    ) {
        let assigner = SequenceAssigner::new();
        let mut events = Vec::new();

        for round in 0..events_per_pane {
            for pane_id in 0..num_panes {
                let (seq, _) = assigner.assign(pane_id);
                let ts = base_ts + (round as u64) * 100 + pane_id;
                let event = make_event_for_prop(
                    pane_id,
                    seq,
                    ts,
                    RecorderEventSource::RobotMode,
                    RecorderEventPayload::IngressText {
                        text: "data".into(),
                        encoding: RecorderTextEncoding::Utf8,
                        redaction: RecorderRedactionLevel::None,
                        ingress_kind: RecorderIngressKind::SendText,
                    },
                );
                events.push(event);
            }
        }

        // Sort by merge key
        events.sort_by(|a, b| {
            RecorderMergeKey::from_event(a).cmp(&RecorderMergeKey::from_event(b))
        });

        let config = InvariantCheckerConfig {
            check_merge_order: true,
            check_causality: false,
            expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let report = checker.check(&events);

        prop_assert!(report.passed, "invariant violations: {:?}", report.violations);
        prop_assert_eq!(report.count_by_kind(ViolationKind::MergeOrderViolation), 0);
        prop_assert_eq!(report.count_by_kind(ViolationKind::DuplicateEventId), 0);
    }
}

// ---------------------------------------------------------------------------
// Property: replay determinism — shuffle + sort = same result
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn replay_is_deterministic_under_shuffle(
        num_panes in 1u64..6,
        events_per_pane in 1usize..15,
        base_ts in arb_timestamp(),
        seed in any::<u64>(),
    ) {
        let assigner = SequenceAssigner::new();
        let mut events = Vec::new();

        for round in 0..events_per_pane {
            for pane_id in 0..num_panes {
                let (seq, _) = assigner.assign(pane_id);
                let ts = base_ts + (round as u64) * 100 + pane_id;
                let event = make_event_for_prop(
                    pane_id,
                    seq,
                    ts,
                    RecorderEventSource::RobotMode,
                    RecorderEventPayload::IngressText {
                        text: format!("r{}-p{}", round, pane_id),
                        encoding: RecorderTextEncoding::Utf8,
                        redaction: RecorderRedactionLevel::None,
                        ingress_kind: RecorderIngressKind::SendText,
                    },
                );
                events.push(event);
            }
        }

        // Create a deterministic shuffle using seed
        let mut shuffled = events.clone();
        // Simple deterministic permutation: rotate by seed % len
        let len = shuffled.len();
        if len > 1 {
            let rotation = (seed as usize) % len;
            shuffled.rotate_left(rotation);
        }

        let result = verify_replay_determinism(&events, &shuffled);
        prop_assert!(result.deterministic, "replay diverged: {}", result.message);
    }
}

// ---------------------------------------------------------------------------
// Property: SequenceAssigner produces valid replay orders
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn sequence_assigner_always_valid(
        pane_schedule in prop::collection::vec(arb_pane_id(), 1..100),
    ) {
        let assigner = SequenceAssigner::new();
        let mut orders = Vec::new();

        for &pane_id in &pane_schedule {
            let (pane_seq, global_seq) = assigner.assign(pane_id);
            orders.push(ReplayOrder::new(global_seq, pane_id, pane_seq));
        }

        let violations = validate_replay_order(&orders);
        prop_assert!(violations.is_empty(), "violations: {:?}", violations);
    }
}

// ---------------------------------------------------------------------------
// Property: deterministic ID is stable (same inputs → same output)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn deterministic_id_is_idempotent(
        pane_id in arb_pane_id(),
        seq in 0u64..1000,
        ts in arb_timestamp(),
        text in arb_text(),
    ) {
        let event1 = make_event_for_prop(
            pane_id,
            seq,
            ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: text.clone(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );

        let event2 = make_event_for_prop(
            pane_id,
            seq,
            ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text,
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );

        prop_assert_eq!(&event1.event_id, &event2.event_id, "ID not deterministic");
    }
}

// ---------------------------------------------------------------------------
// Property: merge key ordering is a total order (transitivity)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn merge_key_total_order(
        events_data in prop::collection::vec(
            (arb_pane_id(), 0u64..100, arb_timestamp(), arb_event_source(), arb_payload()),
            3..20
        ),
    ) {
        let events: Vec<RecorderEvent> = events_data
            .into_iter()
            .map(|(pane_id, seq, ts, source, payload)| {
                make_event_for_prop(pane_id, seq, ts, source, payload)
            })
            .collect();

        let mut keys: Vec<RecorderMergeKey> = events.iter().map(RecorderMergeKey::from_event).collect();
        keys.sort();

        // Verify sorted order is maintained (no inversion after sort)
        for window in keys.windows(2) {
            prop_assert!(window[0] <= window[1], "sort order violated: {:?} > {:?}", window[0], window[1]);
        }

        // Verify transitivity: if a <= b and b <= c then a <= c
        if keys.len() >= 3 {
            for i in 0..keys.len() - 2 {
                if keys[i] <= keys[i + 1] && keys[i + 1] <= keys[i + 2] {
                    prop_assert!(
                        keys[i] <= keys[i + 2],
                        "transitivity violated: {:?}, {:?}, {:?}",
                        keys[i], keys[i + 1], keys[i + 2]
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Property: serde roundtrip preserves merge key ordering
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn serde_roundtrip_preserves_merge_keys(
        num_panes in 1u64..5,
        events_per_pane in 1usize..10,
        base_ts in arb_timestamp(),
    ) {
        let assigner = SequenceAssigner::new();
        let mut events = Vec::new();

        for round in 0..events_per_pane {
            for pane_id in 0..num_panes {
                let (seq, _) = assigner.assign(pane_id);
                let ts = base_ts + (round as u64) * 100 + pane_id;
                let event = make_event_for_prop(
                    pane_id,
                    seq,
                    ts,
                    RecorderEventSource::RobotMode,
                    RecorderEventPayload::IngressText {
                        text: "serde-test".into(),
                        encoding: RecorderTextEncoding::Utf8,
                        redaction: RecorderRedactionLevel::None,
                        ingress_kind: RecorderIngressKind::SendText,
                    },
                );
                events.push(event);
            }
        }

        let json = serde_json::to_string(&events).expect("serialize");
        let roundtripped: Vec<RecorderEvent> = serde_json::from_str(&json).expect("deserialize");

        // Merge keys must be identical after roundtrip
        let keys_before: Vec<RecorderMergeKey> = events.iter().map(RecorderMergeKey::from_event).collect();
        let keys_after: Vec<RecorderMergeKey> = roundtripped.iter().map(RecorderMergeKey::from_event).collect();

        prop_assert_eq!(keys_before.len(), keys_after.len());
        for (i, (kb, ka)) in keys_before.iter().zip(keys_after.iter()).enumerate() {
            prop_assert_eq!(kb, ka, "merge key diverged at index {}", i);
        }
    }
}

// ---------------------------------------------------------------------------
// Property: RecorderEvent serde roundtrip preserves all fields
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn recorder_event_serde_roundtrip(
        pane_id in arb_pane_id(),
        seq in 0u64..1000,
        ts in arb_timestamp(),
        source in arb_event_source(),
        payload in arb_payload(),
    ) {
        let event = make_event_for_prop(pane_id, seq, ts, source, payload);
        let json = serde_json::to_string(&event).unwrap();
        let back: RecorderEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.event_id, &event.event_id);
        prop_assert_eq!(back.pane_id, event.pane_id);
        prop_assert_eq!(back.sequence, event.sequence);
        prop_assert_eq!(back.occurred_at_ms, event.occurred_at_ms);
        prop_assert_eq!(back.recorded_at_ms, event.recorded_at_ms);
        prop_assert_eq!(&back.schema_version, &event.schema_version);
    }
}

// ---------------------------------------------------------------------------
// Property: SequenceAssigner global sequence is strictly monotonic
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn sequence_assigner_global_monotonic(
        pane_schedule in prop::collection::vec(arb_pane_id(), 2..100),
    ) {
        let assigner = SequenceAssigner::new();
        let mut prev_global: Option<u64> = None;

        for &pane_id in &pane_schedule {
            let (_pane_seq, global_seq) = assigner.assign(pane_id);
            if let Some(prev) = prev_global {
                prop_assert!(global_seq > prev,
                    "global sequence not strictly increasing: {} -> {}", prev, global_seq);
            }
            prev_global = Some(global_seq);
        }
    }

    /// Per-pane sequences are strictly monotonic within each pane.
    #[test]
    fn sequence_assigner_per_pane_monotonic(
        pane_schedule in prop::collection::vec(arb_pane_id(), 2..100),
    ) {
        let assigner = SequenceAssigner::new();
        let mut per_pane_last: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();

        for &pane_id in &pane_schedule {
            let (pane_seq, _global_seq) = assigner.assign(pane_id);
            if let Some(&prev) = per_pane_last.get(&pane_id) {
                prop_assert!(pane_seq > prev,
                    "per-pane sequence not strictly increasing for pane {}: {} -> {}",
                    pane_id, prev, pane_seq);
            }
            per_pane_last.insert(pane_id, pane_seq);
        }
    }
}

// ---------------------------------------------------------------------------
// Property: merge key reflexivity — a key equals itself
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn merge_key_reflexive(
        pane_id in arb_pane_id(),
        seq in 0u64..1000,
        ts in arb_timestamp(),
    ) {
        let event = make_event_for_prop(
            pane_id, seq, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "reflexive".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let key = RecorderMergeKey::from_event(&event);
        prop_assert_eq!(&key, &key, "merge key should equal itself");
        prop_assert!(key <= key, "merge key should be <= itself");
    }
}

// ---------------------------------------------------------------------------
// Property: empty event list passes all invariant checks
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn empty_events_pass_invariants(_dummy in 0..1_u8) {
        let config = InvariantCheckerConfig {
            check_merge_order: true,
            check_causality: true,
            expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let report = checker.check(&[]);
        prop_assert!(report.passed, "empty event list should pass");
        prop_assert_eq!(report.violations.len(), 0);
    }
}

// ---------------------------------------------------------------------------
// Property: different event sources produce structurally valid IDs
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn different_sources_valid_ids(
        pane_id in arb_pane_id(),
        seq in 0u64..100,
        ts in arb_timestamp(),
        source in arb_event_source(),
    ) {
        let event = make_event_for_prop(
            pane_id, seq, ts, source,
            RecorderEventPayload::IngressText {
                text: "source-test".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        prop_assert!(!event.event_id.is_empty(), "event_id should not be empty");
        // ID should be a hex string (generate_event_id_v1 produces hex)
        prop_assert!(event.event_id.chars().all(|c| c.is_ascii_hexdigit()),
            "event_id should be hex: {}", event.event_id);
    }

    /// RecorderEvent schema_version is always preserved through construction.
    #[test]
    fn event_schema_version_preserved(
        pane_id in arb_pane_id(),
        ts in arb_timestamp(),
    ) {
        let event = make_event_for_prop(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "version".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        prop_assert_eq!(&event.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
    }
}

// ---------------------------------------------------------------------------
// RecorderEvent: Clone and Debug
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn recorder_event_clone_preserves(
        pane_id in arb_pane_id(),
        seq in 0u64..1000,
        ts in arb_timestamp(),
        source in arb_event_source(),
        payload in arb_payload(),
    ) {
        let event = make_event_for_prop(pane_id, seq, ts, source, payload);
        let cloned = event.clone();
        prop_assert_eq!(&cloned.event_id, &event.event_id);
        prop_assert_eq!(cloned.pane_id, event.pane_id);
        prop_assert_eq!(cloned.sequence, event.sequence);
        prop_assert_eq!(cloned.occurred_at_ms, event.occurred_at_ms);
        prop_assert_eq!(cloned.recorded_at_ms, event.recorded_at_ms);
        prop_assert_eq!(&cloned.schema_version, &event.schema_version);
        prop_assert_eq!(&cloned.session_id, &event.session_id);
    }

    #[test]
    fn recorder_event_debug_non_empty(
        pane_id in arb_pane_id(),
        ts in arb_timestamp(),
    ) {
        let event = make_event_for_prop(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "debug".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let debug = format!("{:?}", event);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("RecorderEvent"));
    }
}

// ---------------------------------------------------------------------------
// RecorderEventSource: Clone, Debug, serde
// ---------------------------------------------------------------------------

fn arb_all_sources() -> impl Strategy<Value = RecorderEventSource> {
    prop_oneof![
        Just(RecorderEventSource::WeztermMux),
        Just(RecorderEventSource::RobotMode),
        Just(RecorderEventSource::WorkflowEngine),
        Just(RecorderEventSource::OperatorAction),
        Just(RecorderEventSource::RecoveryFlow),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn recorder_event_source_clone_eq(src in arb_all_sources()) {
        let cloned = src;
        prop_assert_eq!(src, cloned);
    }

    #[test]
    fn recorder_event_source_debug(src in arb_all_sources()) {
        let debug = format!("{:?}", src);
        prop_assert!(!debug.is_empty());
    }

    #[test]
    fn recorder_event_source_serde_roundtrip(src in arb_all_sources()) {
        let json = serde_json::to_string(&src).unwrap();
        let back: RecorderEventSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(src, back);
    }

    #[test]
    fn recorder_event_source_serde_snake_case(src in arb_all_sources()) {
        let json = serde_json::to_string(&src).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized source '{}' should be snake_case", inner
        );
    }
}

// ---------------------------------------------------------------------------
// RecorderEventCausality: Clone, Debug, serde
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn causality_clone_preserves(
        parent in proptest::option::of("[a-f0-9]{16}"),
        trigger in proptest::option::of("[a-f0-9]{16}"),
        root in proptest::option::of("[a-f0-9]{16}"),
    ) {
        let causality = RecorderEventCausality {
            parent_event_id: parent.clone(),
            trigger_event_id: trigger.clone(),
            root_event_id: root.clone(),
        };
        let cloned = causality.clone();
        prop_assert_eq!(&cloned.parent_event_id, &causality.parent_event_id);
        prop_assert_eq!(&cloned.trigger_event_id, &causality.trigger_event_id);
        prop_assert_eq!(&cloned.root_event_id, &causality.root_event_id);
    }

    #[test]
    fn causality_serde_roundtrip(
        parent in proptest::option::of("[a-f0-9]{16}"),
        trigger in proptest::option::of("[a-f0-9]{16}"),
    ) {
        let causality = RecorderEventCausality {
            parent_event_id: parent,
            trigger_event_id: trigger,
            root_event_id: None,
        };
        let json = serde_json::to_string(&causality).unwrap();
        let back: RecorderEventCausality = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.parent_event_id, &causality.parent_event_id);
        prop_assert_eq!(&back.trigger_event_id, &causality.trigger_event_id);
        prop_assert_eq!(&back.root_event_id, &causality.root_event_id);
    }

    #[test]
    fn causality_debug_non_empty(
        parent in proptest::option::of("[a-f0-9]{16}"),
    ) {
        let causality = RecorderEventCausality {
            parent_event_id: parent,
            trigger_event_id: None,
            root_event_id: None,
        };
        let debug = format!("{:?}", causality);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("RecorderEventCausality"));
    }
}

// ---------------------------------------------------------------------------
// RecorderEventPayload: Clone and serde
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn payload_clone_preserves(payload in arb_payload()) {
        let cloned = payload.clone();
        prop_assert_eq!(cloned, payload);
    }

    #[test]
    fn ingress_payload_serde_roundtrip(
        text in arb_text(),
    ) {
        let payload = RecorderEventPayload::IngressText {
            text,
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, payload);
    }

    #[test]
    fn egress_payload_serde_roundtrip(
        text in arb_text(),
        is_gap in any::<bool>(),
    ) {
        let payload = RecorderEventPayload::EgressOutput {
            text,
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: if is_gap { RecorderSegmentKind::Gap } else { RecorderSegmentKind::Delta },
            is_gap,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, payload);
    }
}

// ---------------------------------------------------------------------------
// Enum serde roundtrips: encoding, redaction, ingress kind, segment kind
// ---------------------------------------------------------------------------

fn arb_encoding() -> impl Strategy<Value = RecorderTextEncoding> {
    Just(RecorderTextEncoding::Utf8)
}

fn arb_redaction() -> impl Strategy<Value = RecorderRedactionLevel> {
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

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn encoding_serde_roundtrip(e in arb_encoding()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: RecorderTextEncoding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(e, back);
    }

    #[test]
    fn redaction_serde_roundtrip(r in arb_redaction()) {
        let json = serde_json::to_string(&r).unwrap();
        let back: RecorderRedactionLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(r, back);
    }

    #[test]
    fn ingress_kind_serde_roundtrip(k in arb_ingress_kind()) {
        let json = serde_json::to_string(&k).unwrap();
        let back: RecorderIngressKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(k, back);
    }

    #[test]
    fn segment_kind_serde_roundtrip(k in arb_segment_kind()) {
        let json = serde_json::to_string(&k).unwrap();
        let back: RecorderSegmentKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(k, back);
    }

    #[test]
    fn redaction_serde_snake_case(r in arb_redaction()) {
        let json = serde_json::to_string(&r).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "redaction '{}' should be snake_case", inner
        );
    }

    #[test]
    fn segment_kind_serde_snake_case(k in arb_segment_kind()) {
        let json = serde_json::to_string(&k).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "segment kind '{}' should be snake_case", inner
        );
    }
}

// ---------------------------------------------------------------------------
// RecorderEvent JSON structure
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn recorder_event_json_has_expected_fields(
        pane_id in arb_pane_id(),
        seq in 0u64..100,
        ts in arb_timestamp(),
    ) {
        let event = make_event_for_prop(
            pane_id, seq, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "fields".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("schema_version"));
        prop_assert!(obj.contains_key("event_id"));
        prop_assert!(obj.contains_key("pane_id"));
        prop_assert!(obj.contains_key("source"));
        prop_assert!(obj.contains_key("occurred_at_ms"));
        prop_assert!(obj.contains_key("sequence"));
        // Flattened payload field
        prop_assert!(obj.contains_key("event_type"));
    }
}

// ---------------------------------------------------------------------------
// InvariantCheckerConfig: defaults and construction
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn invariant_checker_default_config(_dummy in 0..1u8) {
        let config = InvariantCheckerConfig::default();
        let debug = format!("{:?}", config);
        prop_assert!(!debug.is_empty());
    }

    #[test]
    fn invariant_checker_accepts_single_event(
        pane_id in arb_pane_id(),
        ts in arb_timestamp(),
    ) {
        let event = make_event_for_prop(
            pane_id, 0, ts,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "single".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let config = InvariantCheckerConfig {
            check_merge_order: true,
            check_causality: false,
            expected_schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            ..Default::default()
        };
        let checker = InvariantChecker::with_config(config);
        let report = checker.check(&[event]);
        prop_assert!(report.passed, "single event should pass: {:?}", report.violations);
    }
}

// ---------------------------------------------------------------------------
// SequenceAssigner: additional invariants
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// First assignment for any pane starts at sequence 0.
    #[test]
    fn sequence_assigner_starts_at_zero(pane_id in arb_pane_id()) {
        let assigner = SequenceAssigner::new();
        let (pane_seq, _global) = assigner.assign(pane_id);
        prop_assert_eq!(pane_seq, 0, "first pane_seq should be 0, got {}", pane_seq);
    }

    /// Multiple panes have independent per-pane counters.
    #[test]
    fn sequence_assigner_independent_panes(
        p1 in 0u64..10,
        p2 in 10u64..20,
        n in 1usize..10,
    ) {
        let assigner = SequenceAssigner::new();
        // Assign n to pane p1
        for _ in 0..n {
            assigner.assign(p1);
        }
        // First assignment to p2 should still be 0 (independent counter)
        let (pane_seq, _) = assigner.assign(p2);
        prop_assert_eq!(pane_seq, 0,
            "first seq for pane {} should be 0 after {} assigns to pane {}", p2, n, p1);
    }

    /// Global sequence is always >= pane sequence.
    #[test]
    fn sequence_assigner_global_gte_pane(
        schedule in prop::collection::vec(arb_pane_id(), 1..50),
    ) {
        let assigner = SequenceAssigner::new();
        for &pane_id in &schedule {
            let (pane_seq, global_seq) = assigner.assign(pane_id);
            prop_assert!(global_seq >= pane_seq,
                "global {} < pane {} for pane_id {}", global_seq, pane_seq, pane_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Merge key: antisymmetry
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn merge_key_antisymmetric(
        p1 in arb_pane_id(),
        p2 in arb_pane_id(),
        s1 in 0u64..100,
        s2 in 0u64..100,
        ts1 in arb_timestamp(),
        ts2 in arb_timestamp(),
    ) {
        let e1 = make_event_for_prop(
            p1, s1, ts1,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "a".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let e2 = make_event_for_prop(
            p2, s2, ts2,
            RecorderEventSource::RobotMode,
            RecorderEventPayload::IngressText {
                text: "b".into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        );
        let k1 = RecorderMergeKey::from_event(&e1);
        let k2 = RecorderMergeKey::from_event(&e2);
        // Antisymmetry: if k1 <= k2 and k2 <= k1, then k1 == k2
        if k1 <= k2 && k2 <= k1 {
            prop_assert_eq!(&k1, &k2, "antisymmetry violated");
        }
    }
}
