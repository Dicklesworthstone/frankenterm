use frankenterm_core::recorder_storage::RecorderOffset;
use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
};
use frankenterm_core::search::{
    ChunkDirection, ChunkInputEvent, ChunkPolicyConfig, RECORDER_CHUNKING_POLICY_V1,
    build_semantic_chunks,
};
use sha2::{Digest, Sha256};

fn ingress_event(
    event_id: &str,
    pane_id: u64,
    ordinal: u64,
    occurred_at_ms: u64,
    text: &str,
) -> ChunkInputEvent {
    ChunkInputEvent {
        event: RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some(format!("sess-{pane_id}")),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: ordinal,
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
        },
        offset: RecorderOffset {
            segment_id: 1,
            ordinal,
            byte_offset: ordinal * 100,
        },
    }
}

fn egress_event(
    event_id: &str,
    pane_id: u64,
    ordinal: u64,
    occurred_at_ms: u64,
    text: &str,
    is_gap: bool,
) -> ChunkInputEvent {
    ChunkInputEvent {
        event: RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some(format!("sess-{pane_id}")),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: ordinal,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: if is_gap {
                    RecorderSegmentKind::Gap
                } else {
                    RecorderSegmentKind::Delta
                },
                is_gap,
            },
        },
        offset: RecorderOffset {
            segment_id: 1,
            ordinal,
            byte_offset: ordinal * 100,
        },
    }
}

fn control_marker_event(
    event_id: &str,
    pane_id: u64,
    ordinal: u64,
    occurred_at_ms: u64,
) -> ChunkInputEvent {
    ChunkInputEvent {
        event: RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: event_id.to_string(),
            pane_id,
            session_id: Some(format!("sess-{pane_id}")),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WorkflowEngine,
            occurred_at_ms,
            recorded_at_ms: occurred_at_ms + 1,
            sequence: ordinal,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: serde_json::json!({ "marker": "prompt_boundary" }),
            },
        },
        offset: RecorderOffset {
            segment_id: 1,
            ordinal,
            byte_offset: ordinal * 100,
        },
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[test]
fn chunking_is_deterministic_for_same_input() {
    let events = vec![
        ingress_event("evt-1", 7, 1, 1_000, "ls"),
        egress_event("evt-2", 7, 2, 1_010, "file-a\nfile-b", false),
        egress_event("evt-3", 7, 3, 1_020, "done", false),
    ];

    let config = ChunkPolicyConfig::default();
    let run_one = build_semantic_chunks(&events, &config);
    let run_two = build_semantic_chunks(&events, &config);
    assert_eq!(run_one, run_two);
    assert!(!run_one.is_empty());
}

#[test]
fn long_single_egress_is_split_with_overlap() {
    let long_text = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(3);
    let events = vec![egress_event("evt-long", 9, 1, 20_000, &long_text, false)];

    let config = ChunkPolicyConfig {
        max_chunk_chars: 40,
        max_chunk_events: 100,
        max_window_ms: 120_000,
        hard_gap_ms: 30_000,
        min_chunk_chars: 0,
        merge_window_ms: 8_000,
        overlap_chars: 10,
    };

    let chunks = build_semantic_chunks(&events, &config);
    assert!(chunks.len() > 1);
    assert!(
        chunks.iter().skip(1).all(|chunk| chunk.overlap.is_some()),
        "expected overlap metadata on all chunks after the first"
    );
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.direction == ChunkDirection::Egress)
    );
}

#[test]
fn tiny_ingress_glues_with_immediate_egress() {
    let events = vec![
        ingress_event("evt-in", 3, 1, 1_000, "git status"),
        egress_event("evt-out", 3, 2, 1_200, "On branch main", false),
    ];

    let chunks = build_semantic_chunks(&events, &ChunkPolicyConfig::default());
    assert_eq!(chunks.len(), 1);
    let only = &chunks[0];
    assert_eq!(only.direction, ChunkDirection::MixedGlued);
    assert!(only.text.contains("[IN] git status"));
    assert!(only.text.contains("[OUT] On branch main"));
}

#[test]
fn hard_gap_prevents_glue() {
    let events = vec![
        ingress_event("evt-in", 3, 1, 1_000, "git status"),
        egress_event("evt-out", 3, 2, 100_000, "On branch main", false),
    ];

    let config = ChunkPolicyConfig {
        hard_gap_ms: 10_000,
        ..ChunkPolicyConfig::default()
    };
    let chunks = build_semantic_chunks(&events, &config);
    assert_eq!(chunks.len(), 2);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.direction != ChunkDirection::MixedGlued)
    );
}

#[test]
fn gap_marker_forces_boundary() {
    let events = vec![
        egress_event("evt-1", 11, 1, 1_000, "line one", false),
        egress_event("evt-gap", 11, 2, 1_005, "", true),
        egress_event("evt-2", 11, 3, 1_010, "line two", false),
    ];

    let chunks = build_semantic_chunks(&events, &ChunkPolicyConfig::default());
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].start_offset.ordinal, 1);
    assert_eq!(chunks[1].start_offset.ordinal, 3);
}

#[test]
fn control_marker_forces_boundary() {
    let events = vec![
        ingress_event("evt-1", 5, 1, 10_000, "echo hi"),
        control_marker_event("evt-marker", 5, 2, 10_100),
        egress_event("evt-2", 5, 3, 10_200, "hi", false),
    ];

    let chunks = build_semantic_chunks(&events, &ChunkPolicyConfig::default());
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].end_offset.ordinal, 1);
    assert_eq!(chunks[1].start_offset.ordinal, 3);
}

#[test]
fn chunk_metadata_is_traceable_and_hash_stable() {
    let events = vec![
        ingress_event("evt-in", 17, 1, 50_000, "cargo test"),
        egress_event("evt-out", 17, 2, 50_100, "running 2 tests", false),
    ];
    let config = ChunkPolicyConfig {
        overlap_chars: 0,
        min_chunk_chars: 0,
        ..ChunkPolicyConfig::default()
    };

    let chunks = build_semantic_chunks(&events, &config);
    assert_eq!(chunks.len(), 2);

    let ingress_chunk = &chunks[0];
    assert_eq!(ingress_chunk.policy_version, RECORDER_CHUNKING_POLICY_V1);
    assert_eq!(ingress_chunk.start_offset.ordinal, 1);
    assert_eq!(ingress_chunk.end_offset.ordinal, 1);
    assert_eq!(ingress_chunk.event_ids, vec!["evt-in".to_string()]);
    assert_eq!(
        ingress_chunk.content_hash,
        sha256_hex(ingress_chunk.text.as_bytes())
    );
    assert_eq!(ingress_chunk.chunk_id.len(), 64);

    let egress_chunk = &chunks[1];
    assert_eq!(egress_chunk.start_offset.ordinal, 2);
    assert_eq!(egress_chunk.end_offset.ordinal, 2);
    assert_eq!(egress_chunk.event_ids, vec!["evt-out".to_string()]);
    assert_eq!(
        egress_chunk.content_hash,
        sha256_hex(egress_chunk.text.as_bytes())
    );
    assert_eq!(egress_chunk.chunk_id.len(), 64);
}
