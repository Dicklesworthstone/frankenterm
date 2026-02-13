//! Property-based tests for the `recording` module.
//!
//! Validates serde roundtrips, binary encoding, monotonic counters,
//! mapping functions, and schema parsing for all recording types.
//!
//! Pure logic only -- no filesystem I/O, no async, no tempfiles.

use proptest::prelude::*;
use serde_json;

use frankenterm_core::ingest::CapturedSegmentKind;
use frankenterm_core::policy::{ActionKind, ActorKind};
use frankenterm_core::recording::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_frame_type() -> impl Strategy<Value = FrameType> {
    prop_oneof![
        Just(FrameType::Output),
        Just(FrameType::Resize),
        Just(FrameType::Event),
        Just(FrameType::Marker),
        Just(FrameType::Input),
    ]
}

fn arb_delta_encoding() -> impl Strategy<Value = DeltaEncoding> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..64).prop_map(DeltaEncoding::Full),
        (0u32..1000, arb_diff_ops()).prop_map(|(base, ops)| DeltaEncoding::Diff {
            base_frame: base,
            ops,
        }),
        (0u32..1000).prop_map(|base| DeltaEncoding::Repeat { base_frame: base }),
    ]
}

fn arb_diff_op() -> impl Strategy<Value = DiffOp> {
    prop_oneof![
        (0u32..1000, 1u32..500).prop_map(|(offset, len)| DiffOp::Copy { offset, len }),
        proptest::collection::vec(any::<u8>(), 0..32).prop_map(|data| DiffOp::Insert { data }),
    ]
}

fn arb_diff_ops() -> impl Strategy<Value = Vec<DiffOp>> {
    proptest::collection::vec(arb_diff_op(), 0..8)
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

fn arb_causality() -> impl Strategy<Value = RecorderEventCausality> {
    (
        proptest::option::of("[a-z0-9]{8}"),
        proptest::option::of("[a-z0-9]{8}"),
        proptest::option::of("[a-z0-9]{8}"),
    )
        .prop_map(|(parent, trigger, root)| RecorderEventCausality {
            parent_event_id: parent,
            trigger_event_id: trigger,
            root_event_id: root,
        })
}

fn arb_event_payload() -> impl Strategy<Value = RecorderEventPayload> {
    prop_oneof![
        (
            "[a-zA-Z0-9 ]{0,30}",
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
        (
            "[a-zA-Z0-9 ]{0,30}",
            arb_text_encoding(),
            arb_redaction_level(),
            arb_segment_kind(),
            any::<bool>(),
        )
            .prop_map(|(text, encoding, redaction, segment_kind, is_gap)| {
                RecorderEventPayload::EgressOutput {
                    text,
                    encoding,
                    redaction,
                    segment_kind,
                    is_gap,
                }
            }),
        arb_control_marker_type().prop_map(|cmt| RecorderEventPayload::ControlMarker {
            control_marker_type: cmt,
            details: serde_json::json!({}),
        }),
        (arb_lifecycle_phase(), proptest::option::of("[a-z ]{0,20}")).prop_map(
            |(phase, reason)| RecorderEventPayload::LifecycleMarker {
                lifecycle_phase: phase,
                reason,
                details: serde_json::json!({}),
            }
        ),
    ]
}

fn arb_recorder_event() -> impl Strategy<Value = RecorderEvent> {
    (
        "[a-z0-9]{16}",
        0u64..1000,
        proptest::option::of("[a-z0-9]{8}"),
        proptest::option::of("[a-z0-9]{8}"),
        proptest::option::of("[a-z0-9]{8}"),
        arb_event_source(),
        0u64..2_000_000,
        0u64..2_000_000,
        0u64..10_000,
        arb_causality(),
        arb_event_payload(),
    )
        .prop_map(
            |(
                event_id,
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
            )| {
                RecorderEvent {
                    schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
                    event_id,
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

fn arb_frame_header() -> impl Strategy<Value = FrameHeader> {
    (any::<u64>(), arb_frame_type(), any::<u8>(), 0u32..10_000).prop_map(|(ts, ft, flags, plen)| {
        FrameHeader {
            timestamp_ms: ts,
            frame_type: ft,
            flags,
            payload_len: plen,
        }
    })
}

fn arb_recording_frame() -> impl Strategy<Value = RecordingFrame> {
    (
        arb_frame_type(),
        any::<u8>(),
        proptest::collection::vec(any::<u8>(), 0..256),
    )
        .prop_map(|(ft, flags, payload)| RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 12345,
                frame_type: ft,
                flags,
                payload_len: payload.len() as u32,
            },
            payload,
        })
}

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::SendCtrlZ),
        Just(ActionKind::SendControl),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Activate),
        Just(ActionKind::Close),
        Just(ActionKind::BrowserAuth),
        Just(ActionKind::WorkflowRun),
        Just(ActionKind::ReservePane),
        Just(ActionKind::ReleasePane),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
    ]
}

fn arb_ingress_outcome() -> impl Strategy<Value = IngressOutcome> {
    prop_oneof![
        Just(IngressOutcome::Allowed),
        "[a-zA-Z0-9 ]{1,30}".prop_map(|r| IngressOutcome::Denied { reason: r }),
        Just(IngressOutcome::RequiresApproval),
        "[a-zA-Z0-9 ]{1,30}".prop_map(|e| IngressOutcome::Error { error: e }),
    ]
}

// =============================================================================
// 1. FrameType serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn frame_type_serde_roundtrip(ft in arb_frame_type()) {
        let json = serde_json::to_string(&ft).unwrap();
        let back: FrameType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ft, back, "FrameType roundtrip failed for {:?}", ft);
    }
}

// =============================================================================
// 2. DeltaEncoding serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn delta_encoding_serde_roundtrip(enc in arb_delta_encoding()) {
        let json = serde_json::to_string(&enc).unwrap();
        let back: DeltaEncoding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(enc, back);
    }
}

// =============================================================================
// 3. DiffOp serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn diff_op_serde_roundtrip(op in arb_diff_op()) {
        let json = serde_json::to_string(&op).unwrap();
        let back: DiffOp = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(op, back);
    }
}

// =============================================================================
// 4. RecordingFrame::encode() produces exactly 14 + payload.len() bytes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn encode_length_is_14_plus_payload(frame in arb_recording_frame()) {
        let encoded = frame.encode();
        let expected = 14 + frame.payload.len();
        prop_assert_eq!(encoded.len(), expected, "encode len mismatch: got {}, expected {}", encoded.len(), expected);
    }
}

// =============================================================================
// 5. RecordingFrame::encode() round-trip: parse bytes back to header fields
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn encode_roundtrip_header_fields(
        ts in any::<u64>(),
        ft in arb_frame_type(),
        flags in any::<u8>(),
        payload in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: ts,
                frame_type: ft,
                flags,
                payload_len: payload.len() as u32,
            },
            payload: payload.clone(),
        };

        let bytes = frame.encode();

        // Parse timestamp
        let parsed_ts = u64::from_le_bytes(bytes[0..8].try_into().unwrap());
        prop_assert_eq!(parsed_ts, ts, "timestamp mismatch: got {}, expected {}", parsed_ts, ts);

        // Parse frame_type byte
        let ft_byte = bytes[8];
        prop_assert_eq!(ft_byte, ft as u8, "frame_type byte mismatch");

        // Parse flags
        let parsed_flags = bytes[9];
        prop_assert_eq!(parsed_flags, flags, "flags mismatch");

        // Parse payload_len
        let parsed_plen = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
        prop_assert_eq!(parsed_plen, payload.len() as u32, "payload_len mismatch");

        // Parse payload
        prop_assert_eq!(&bytes[14..], payload.as_slice(), "payload mismatch");
    }
}

// =============================================================================
// 6. RecordingOptions::default() values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn recording_options_default_values(_dummy in Just(())) {
        let opts = RecordingOptions::default();
        prop_assert_eq!(opts.flush_threshold, 64, "flush_threshold should be 64");
        prop_assert!(opts.redact_output, "redact_output should be true");
        prop_assert!(opts.redact_events, "redact_events should be true");
    }
}

// =============================================================================
// 7. RecorderEventSource serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn event_source_serde_roundtrip(src in arb_event_source()) {
        let json = serde_json::to_string(&src).unwrap();
        let back: RecorderEventSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(src, back);
    }
}

// =============================================================================
// 8. RecorderEventSource snake_case serialization
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn event_source_serializes_snake_case(src in arb_event_source()) {
        let json = serde_json::to_string(&src).unwrap();
        // All snake_case values should be lowercase with underscores, no uppercase
        let inner = json.trim_matches('"');
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "expected snake_case, got: {}", inner);
    }
}

// =============================================================================
// 9. RecorderTextEncoding serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn text_encoding_serde_roundtrip(enc in arb_text_encoding()) {
        let json = serde_json::to_string(&enc).unwrap();
        let back: RecorderTextEncoding = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(enc, back);
        let inner = json.trim_matches('"');
        prop_assert_eq!(inner, "utf8");
    }
}

// =============================================================================
// 10. RecorderRedactionLevel serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn redaction_level_serde_roundtrip(level in arb_redaction_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: RecorderRedactionLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, back);
        let inner = json.trim_matches('"');
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "expected snake_case, got: {}", inner);
    }
}

// =============================================================================
// 11. RecorderIngressKind serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn ingress_kind_serde_roundtrip(kind in arb_ingress_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: RecorderIngressKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
        let inner = json.trim_matches('"');
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "expected snake_case, got: {}", inner);
    }
}

// =============================================================================
// 12. RecorderSegmentKind serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn segment_kind_serde_roundtrip(kind in arb_segment_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: RecorderSegmentKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
        let inner = json.trim_matches('"');
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "expected snake_case, got: {}", inner);
    }
}

// =============================================================================
// 13. RecorderControlMarkerType serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn control_marker_type_serde_roundtrip(cmt in arb_control_marker_type()) {
        let json = serde_json::to_string(&cmt).unwrap();
        let back: RecorderControlMarkerType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cmt, back);
        let inner = json.trim_matches('"');
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "expected snake_case, got: {}", inner);
    }
}

// =============================================================================
// 14. RecorderLifecyclePhase serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn lifecycle_phase_serde_roundtrip(phase in arb_lifecycle_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let back: RecorderLifecyclePhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(phase, back);
        let inner = json.trim_matches('"');
        let is_snake = inner.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "expected snake_case, got: {}", inner);
    }
}

// =============================================================================
// 15. RecorderEventCausality serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn causality_serde_roundtrip(c in arb_causality()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: RecorderEventCausality = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c, back);
    }
}

// =============================================================================
// 16. RecorderEventPayload internally-tagged serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn event_payload_serde_roundtrip(payload in arb_event_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(payload, back);
    }
}

// =============================================================================
// 17. RecorderEventPayload contains event_type tag
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn event_payload_has_event_type_tag(payload in arb_event_payload()) {
        let json = serde_json::to_string(&payload).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();
        let has_tag = raw.get("event_type").is_some();
        prop_assert!(has_tag, "payload JSON missing event_type tag: {}", json);

        let tag = raw["event_type"].as_str().unwrap();
        let is_snake = tag.chars().all(|c| c.is_ascii_lowercase() || c == '_');
        prop_assert!(is_snake, "event_type not snake_case: {}", tag);
    }
}

// =============================================================================
// 18. RecorderEvent with flatten payload roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn recorder_event_serde_roundtrip(event in arb_recorder_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let back: RecorderEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event.event_id, back.event_id);
        prop_assert_eq!(event.pane_id, back.pane_id);
        prop_assert_eq!(event.source, back.source);
        prop_assert_eq!(event.occurred_at_ms, back.occurred_at_ms);
        prop_assert_eq!(event.sequence, back.sequence);
        prop_assert_eq!(event.causality, back.causality);
        prop_assert_eq!(event.payload, back.payload);
        prop_assert_eq!(event.schema_version, back.schema_version);
    }
}

// =============================================================================
// 19. RecorderEvent JSON has flattened fields (no nested payload object)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn recorder_event_json_is_flat(event in arb_recorder_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&json).unwrap();

        // event_type should be at the top level (not nested under "payload")
        let has_event_type = raw.get("event_type").is_some();
        prop_assert!(has_event_type, "flattened event must have event_type at top level");

        let no_payload_key = raw.get("payload").is_none();
        prop_assert!(no_payload_key, "flattened event should not have a 'payload' key");
    }
}

// =============================================================================
// 20. parse_recorder_event_json accepts valid events
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn parse_recorder_event_json_accepts_valid(event in arb_recorder_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let parsed = parse_recorder_event_json(&json);
        let is_ok = parsed.is_ok();
        prop_assert!(is_ok, "valid event failed to parse: {}", json);
    }
}

// =============================================================================
// 21. parse_recorder_event_json rejects wrong schema version
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn parse_recorder_event_json_rejects_wrong_version(
        event in arb_recorder_event(),
        bad_version in "[a-z]{3,10}\\.[a-z]{3,10}\\.[a-z]{1,4}",
    ) {
        let mut json_val: serde_json::Value = serde_json::to_value(&event).unwrap();
        json_val["schema_version"] = serde_json::Value::String(bad_version.clone());
        let json = serde_json::to_string(&json_val).unwrap();
        let result = parse_recorder_event_json(&json);
        let is_err = result.is_err();
        prop_assert!(is_err, "should reject version {}", bad_version);
    }
}

// =============================================================================
// 22. parse_recorder_event_json rejects missing schema version
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn parse_recorder_event_json_rejects_missing_version(event in arb_recorder_event()) {
        let mut json_val: serde_json::Value = serde_json::to_value(&event).unwrap();
        let obj = json_val.as_object_mut().unwrap();
        obj.remove("schema_version");
        let json = serde_json::to_string(&json_val).unwrap();
        let result = parse_recorder_event_json(&json);
        let is_err = result.is_err();
        prop_assert!(is_err, "should reject missing schema_version");
    }
}

// =============================================================================
// 23. IngressSequence monotonicity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ingress_sequence_monotonic(count in 1usize..500) {
        let seq = IngressSequence::new();
        let mut prev = seq.next();
        for _ in 1..count {
            let curr = seq.next();
            prop_assert!(curr > prev, "sequence not monotonic: {} <= {}", curr, prev);
            prev = curr;
        }
    }
}

// =============================================================================
// 24. IngressSequence starts at zero
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn ingress_sequence_starts_at_zero(_dummy in Just(())) {
        let seq = IngressSequence::new();
        let first = seq.next();
        prop_assert_eq!(first, 0u64, "first sequence should be 0");
    }
}

// =============================================================================
// 25. IngressSequence::default() behaves same as new()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn ingress_sequence_default_starts_at_zero(_dummy in Just(())) {
        let seq = IngressSequence::default();
        let first = seq.next();
        prop_assert_eq!(first, 0u64, "default sequence should start at 0");
    }
}

// =============================================================================
// 26. GlobalSequence monotonicity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn global_sequence_monotonic(count in 1usize..500) {
        let seq = GlobalSequence::new();
        let mut prev = seq.next();
        for _ in 1..count {
            let curr = seq.next();
            prop_assert!(curr > prev, "global sequence not monotonic: {} <= {}", curr, prev);
            prev = curr;
        }
    }
}

// =============================================================================
// 27. GlobalSequence starts at zero, default same as new
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn global_sequence_starts_at_zero(_dummy in Just(())) {
        let seq_new = GlobalSequence::new();
        let seq_def = GlobalSequence::default();
        prop_assert_eq!(seq_new.next(), 0u64, "new() should start at 0");
        prop_assert_eq!(seq_def.next(), 0u64, "default() should start at 0");
    }
}

// =============================================================================
// 28. actor_to_source covers all ActorKind variants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn actor_to_source_all_variants(actor in arb_actor_kind()) {
        let source = actor_to_source(actor);
        match actor {
            ActorKind::Human => prop_assert_eq!(source, RecorderEventSource::OperatorAction),
            ActorKind::Robot => prop_assert_eq!(source, RecorderEventSource::RobotMode),
            ActorKind::Mcp => prop_assert_eq!(source, RecorderEventSource::RobotMode),
            ActorKind::Workflow => prop_assert_eq!(source, RecorderEventSource::WorkflowEngine),
        }
    }
}

// =============================================================================
// 29. action_to_ingress_kind: SendText with Workflow -> WorkflowAction
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn action_to_ingress_kind_sendtext(actor in arb_actor_kind()) {
        let kind = action_to_ingress_kind(ActionKind::SendText, actor);
        if actor == ActorKind::Workflow {
            prop_assert_eq!(kind, RecorderIngressKind::WorkflowAction);
        } else {
            prop_assert_eq!(kind, RecorderIngressKind::SendText);
        }
    }
}

// =============================================================================
// 30. action_to_ingress_kind: control keys
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn action_to_ingress_kind_control_keys(actor in arb_actor_kind()) {
        let ctrl_actions = [
            ActionKind::SendCtrlC,
            ActionKind::SendCtrlD,
            ActionKind::SendCtrlZ,
            ActionKind::SendControl,
        ];
        for action in ctrl_actions {
            let kind = action_to_ingress_kind(action, actor);
            if actor == ActorKind::Workflow {
                prop_assert_eq!(kind, RecorderIngressKind::WorkflowAction,
                    "ctrl action {:?} with Workflow should be WorkflowAction", action);
            } else {
                prop_assert_eq!(kind, RecorderIngressKind::SendText,
                    "ctrl action {:?} with {:?} should be SendText", action, actor);
            }
        }
    }
}

// =============================================================================
// 31. action_to_ingress_kind: non-injection actions default to SendText
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn action_to_ingress_kind_non_injection(actor in arb_actor_kind()) {
        let non_injection = [
            ActionKind::Spawn,
            ActionKind::Split,
            ActionKind::Activate,
            ActionKind::Close,
            ActionKind::BrowserAuth,
            ActionKind::WorkflowRun,
        ];
        for action in non_injection {
            let kind = action_to_ingress_kind(action, actor);
            prop_assert_eq!(kind, RecorderIngressKind::SendText,
                "non-injection {:?} with {:?} should default to SendText", action, actor);
        }
    }
}

// =============================================================================
// 32. captured_kind_to_segment: Delta mapping
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn captured_kind_delta_maps_correctly(_dummy in Just(())) {
        let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Delta);
        prop_assert_eq!(kind, RecorderSegmentKind::Delta);
        prop_assert!(!is_gap, "Delta should not be a gap");
    }
}

// =============================================================================
// 33. captured_kind_to_segment: Gap mapping
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn captured_kind_gap_maps_correctly(reason in "[a-zA-Z ]{1,20}") {
        let (kind, is_gap) = captured_kind_to_segment(&CapturedSegmentKind::Gap {
            reason: reason.clone(),
        });
        prop_assert_eq!(kind, RecorderSegmentKind::Gap);
        prop_assert!(is_gap, "Gap should have is_gap=true");
    }
}

// =============================================================================
// 34. IngressOutcome variants preserve data
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ingress_outcome_preserves_data(outcome in arb_ingress_outcome()) {
        // Clone and verify structural equality
        let cloned = outcome.clone();
        prop_assert_eq!(outcome, cloned);
    }
}

// =============================================================================
// 35. IngressOutcome::Denied preserves reason string
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ingress_outcome_denied_reason(reason in "[a-zA-Z0-9 ]{1,50}") {
        let outcome = IngressOutcome::Denied { reason: reason.clone() };
        let is_denied = matches!(&outcome, IngressOutcome::Denied { .. });
        prop_assert!(is_denied, "should be Denied variant");
        if let IngressOutcome::Denied { reason: r } = &outcome {
            prop_assert_eq!(r, &reason, "reason mismatch");
        }
    }
}

// =============================================================================
// 36. IngressOutcome::Error preserves error string
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ingress_outcome_error_message(error in "[a-zA-Z0-9 ]{1,50}") {
        let outcome = IngressOutcome::Error { error: error.clone() };
        let is_error = matches!(&outcome, IngressOutcome::Error { .. });
        prop_assert!(is_error, "should be Error variant");
        if let IngressOutcome::Error { error: e } = &outcome {
            prop_assert_eq!(e, &error, "error mismatch");
        }
    }
}

// =============================================================================
// 37. epoch_ms_now returns reasonable range
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    #[test]
    fn epoch_ms_now_reasonable(_dummy in Just(())) {
        let ms = epoch_ms_now();
        // After 2025-01-01 and before 2100-01-01
        prop_assert!(ms > 1_735_689_600_000, "epoch_ms_now too small: {}", ms);
        prop_assert!(ms < 4_102_444_800_000, "epoch_ms_now too large: {}", ms);
    }
}

// =============================================================================
// 38. RECORDER_EVENT_SCHEMA_VERSION_V1 is correct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn schema_version_constant(_dummy in Just(())) {
        prop_assert_eq!(RECORDER_EVENT_SCHEMA_VERSION_V1, "ft.recorder.event.v1");
    }
}

// =============================================================================
// 39. RecorderState variants are distinct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn recorder_state_variants_distinct(_dummy in Just(())) {
        let states = [
            RecorderState::Idle,
            RecorderState::Recording,
            RecorderState::Paused,
            RecorderState::Stopped,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                let ne = states[i] != states[j];
                prop_assert!(ne, "states {:?} and {:?} should be different", states[i], states[j]);
            }
        }
    }
}

// =============================================================================
// 40. RecorderEventPayload IngressText specific fields roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ingress_text_payload_fields(
        text in "[a-zA-Z0-9]{0,30}",
        redaction in arb_redaction_level(),
        ingress_kind in arb_ingress_kind(),
    ) {
        let payload = RecorderEventPayload::IngressText {
            text: text.clone(),
            encoding: RecorderTextEncoding::Utf8,
            redaction,
            ingress_kind,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        if let RecorderEventPayload::IngressText {
            text: t,
            encoding: e,
            redaction: r,
            ingress_kind: ik,
        } = back
        {
            prop_assert_eq!(t, text, "text mismatch");
            prop_assert_eq!(e, RecorderTextEncoding::Utf8);
            prop_assert_eq!(r, redaction);
            prop_assert_eq!(ik, ingress_kind);
        } else {
            prop_assert!(false, "expected IngressText variant");
        }
    }
}

// =============================================================================
// 41. RecorderEventPayload EgressOutput specific fields roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn egress_output_payload_fields(
        text in "[a-zA-Z0-9]{0,30}",
        redaction in arb_redaction_level(),
        segment_kind in arb_segment_kind(),
        is_gap in any::<bool>(),
    ) {
        let payload = RecorderEventPayload::EgressOutput {
            text: text.clone(),
            encoding: RecorderTextEncoding::Utf8,
            redaction,
            segment_kind,
            is_gap,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        if let RecorderEventPayload::EgressOutput {
            text: t,
            encoding: e,
            redaction: r,
            segment_kind: sk,
            is_gap: ig,
        } = back
        {
            prop_assert_eq!(t, text, "text mismatch");
            prop_assert_eq!(e, RecorderTextEncoding::Utf8);
            prop_assert_eq!(r, redaction);
            prop_assert_eq!(sk, segment_kind);
            prop_assert_eq!(ig, is_gap, "is_gap mismatch");
        } else {
            prop_assert!(false, "expected EgressOutput variant");
        }
    }
}

// =============================================================================
// 42. RecorderEventPayload ControlMarker roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn control_marker_payload_roundtrip(cmt in arb_control_marker_type()) {
        let payload = RecorderEventPayload::ControlMarker {
            control_marker_type: cmt,
            details: serde_json::json!({"key": "value"}),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        if let RecorderEventPayload::ControlMarker {
            control_marker_type: ct,
            details: d,
        } = back
        {
            prop_assert_eq!(ct, cmt);
            prop_assert_eq!(d, serde_json::json!({"key": "value"}));
        } else {
            prop_assert!(false, "expected ControlMarker variant");
        }
    }
}

// =============================================================================
// 43. RecorderEventPayload LifecycleMarker roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn lifecycle_marker_payload_roundtrip(
        phase in arb_lifecycle_phase(),
        reason in proptest::option::of("[a-z ]{0,20}"),
    ) {
        let payload = RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: phase,
            reason: reason.clone(),
            details: serde_json::json!(null),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: RecorderEventPayload = serde_json::from_str(&json).unwrap();
        if let RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: p,
            reason: r,
            details: d,
        } = back
        {
            prop_assert_eq!(p, phase);
            prop_assert_eq!(r, reason);
            prop_assert_eq!(d, serde_json::json!(null));
        } else {
            prop_assert!(false, "expected LifecycleMarker variant");
        }
    }
}

// =============================================================================
// 44. parse_recorder_event_json tolerates unknown fields
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn parse_recorder_event_json_tolerates_extra_fields(event in arb_recorder_event()) {
        let mut json_val: serde_json::Value = serde_json::to_value(&event).unwrap();
        // Add an unknown field
        json_val["future_field_xyz"] = serde_json::json!("unknown_value");
        json_val["another_future"] = serde_json::json!(42);
        let json = serde_json::to_string(&json_val).unwrap();
        let result = parse_recorder_event_json(&json);
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "should tolerate unknown fields: {}", json);
    }
}

// =============================================================================
// 45. RecordingFrame encode: timestamp endianness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn encode_timestamp_little_endian(ts in any::<u64>()) {
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: ts,
                frame_type: FrameType::Output,
                flags: 0,
                payload_len: 0,
            },
            payload: vec![],
        };
        let bytes = frame.encode();
        let expected_bytes = ts.to_le_bytes();
        prop_assert_eq!(&bytes[0..8], &expected_bytes[..], "timestamp LE mismatch");
    }
}

// =============================================================================
// 46. RecordingFrame encode: payload_len endianness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn encode_payload_len_little_endian(payload in proptest::collection::vec(any::<u8>(), 0..500)) {
        let plen = payload.len() as u32;
        let frame = RecordingFrame {
            header: FrameHeader {
                timestamp_ms: 0,
                frame_type: FrameType::Marker,
                flags: 0,
                payload_len: plen,
            },
            payload,
        };
        let bytes = frame.encode();
        let expected_bytes = plen.to_le_bytes();
        prop_assert_eq!(&bytes[10..14], &expected_bytes[..], "payload_len LE mismatch");
    }
}

// =============================================================================
// 47. FrameType repr(u8) values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn frame_type_repr_values(_dummy in Just(())) {
        prop_assert_eq!(FrameType::Output as u8, 1u8);
        prop_assert_eq!(FrameType::Resize as u8, 2u8);
        prop_assert_eq!(FrameType::Event as u8, 3u8);
        prop_assert_eq!(FrameType::Marker as u8, 4u8);
        prop_assert_eq!(FrameType::Input as u8, 5u8);
    }
}

// =============================================================================
// 48. RecorderEvent schema_version is preserved through parse
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn recorder_event_schema_version_preserved(event in arb_recorder_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let parsed = parse_recorder_event_json(&json).unwrap();
        prop_assert_eq!(
            parsed.schema_version,
            RECORDER_EVENT_SCHEMA_VERSION_V1,
            "schema_version should be preserved"
        );
    }
}
