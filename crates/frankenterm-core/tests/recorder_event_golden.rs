//! Exact-byte goldens for recorder append-log event records.
//!
//! The append-log backend stores each [`RecorderEvent`] as:
//!
//! 1. a 4-byte little-endian `u32` payload length
//! 2. compact `serde_json::to_vec(&event)` bytes
//!
//! Regenerate intentionally with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test recorder_event_golden
//! ```

use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderLifecyclePhase, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    parse_recorder_event_json,
};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn golden_dir() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("goldens")
        .join("recorder_events")
}

fn golden_path(name: &str) -> PathBuf {
    golden_dir().join(format!("{name}.bin"))
}

fn base_event(
    event_id: &str,
    pane_id: u64,
    sequence: u64,
    source: RecorderEventSource,
    payload: RecorderEventPayload,
) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: event_id.to_string(),
        pane_id,
        session_id: Some("sess-recorder-golden".to_string()),
        workflow_id: Some("wf-recorder-golden".to_string()),
        correlation_id: Some("corr-recorder-golden".to_string()),
        source,
        occurred_at_ms: 1_777_100_000_000 + sequence,
        recorded_at_ms: 1_777_100_000_100 + sequence,
        sequence,
        causality: RecorderEventCausality {
            parent_event_id: if sequence == 0 {
                None
            } else {
                Some(format!("rec-golden-{:02}", sequence - 1))
            },
            trigger_event_id: Some("trigger-recorder-golden".to_string()),
            root_event_id: Some("rec-golden-00".to_string()),
        },
        payload,
    }
}

fn recorder_event_fixtures() -> Vec<(&'static str, RecorderEvent)> {
    vec![
        (
            "pane_start",
            base_event(
                "rec-golden-00",
                41,
                0,
                RecorderEventSource::WeztermMux,
                RecorderEventPayload::LifecycleMarker {
                    lifecycle_phase: RecorderLifecyclePhase::PaneOpened,
                    reason: Some("pane discovered by mux poll".to_string()),
                    details: json!({
                        "domain": "local",
                        "title": "cod_4",
                        "cwd": "/Users/jemanuel/projects/frankenterm"
                    }),
                },
            ),
        ),
        (
            "capture",
            base_event(
                "rec-golden-01",
                41,
                1,
                RecorderEventSource::WeztermMux,
                RecorderEventPayload::EgressOutput {
                    text: "running 5 tests\nok\n".to_string(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::None,
                    segment_kind: RecorderSegmentKind::Delta,
                    is_gap: false,
                },
            ),
        ),
        (
            "error",
            base_event(
                "rec-golden-02",
                41,
                2,
                RecorderEventSource::RobotMode,
                RecorderEventPayload::ControlMarker {
                    control_marker_type: RecorderControlMarkerType::PolicyDecision,
                    details: json!({
                        "outcome": "error",
                        "error_code": "robot.pane_not_found",
                        "message": "pane 99 not found"
                    }),
                },
            ),
        ),
        (
            "ack",
            base_event(
                "rec-golden-03",
                41,
                3,
                RecorderEventSource::WorkflowEngine,
                RecorderEventPayload::ControlMarker {
                    control_marker_type: RecorderControlMarkerType::ApprovalCheckpoint,
                    details: json!({
                        "approval_id": "appr-rec-golden",
                        "status": "acknowledged",
                        "acked_by": "operator"
                    }),
                },
            ),
        ),
        (
            "pane_end",
            base_event(
                "rec-golden-04",
                41,
                4,
                RecorderEventSource::WeztermMux,
                RecorderEventPayload::LifecycleMarker {
                    lifecycle_phase: RecorderLifecyclePhase::PaneClosed,
                    reason: Some("pane exited with status 0".to_string()),
                    details: json!({
                        "exit_status": 0,
                        "last_sequence": 3,
                        "tail_bytes_captured": 19
                    }),
                },
            ),
        ),
        (
            "ingress_send",
            base_event(
                "rec-golden-05",
                41,
                5,
                RecorderEventSource::RobotMode,
                RecorderEventPayload::IngressText {
                    text: "cargo test recorder\n".to_string(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::Partial,
                    ingress_kind: RecorderIngressKind::SendText,
                },
            ),
        ),
        (
            "redacted_snapshot",
            base_event(
                "rec-golden-06",
                41,
                6,
                RecorderEventSource::WeztermMux,
                RecorderEventPayload::EgressOutput {
                    text: "[redacted snapshot]\n".to_string(),
                    encoding: RecorderTextEncoding::Utf8,
                    redaction: RecorderRedactionLevel::Full,
                    segment_kind: RecorderSegmentKind::Snapshot,
                    is_gap: false,
                },
            ),
        ),
    ]
}

fn encode_append_log_record(event: &RecorderEvent) -> Vec<u8> {
    let payload = serde_json::to_vec(event).expect("serialize recorder event");
    let len = u32::try_from(payload.len()).expect("golden payload fits u32");
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn decode_append_log_record(bytes: &[u8]) -> RecorderEvent {
    assert!(
        bytes.len() >= 4,
        "append-log record must include length prefix"
    );
    let payload_len = u32::from_le_bytes(bytes[0..4].try_into().expect("4-byte length")) as usize;
    assert_eq!(
        payload_len,
        bytes.len() - 4,
        "length prefix must match payload byte count"
    );
    let payload = std::str::from_utf8(&bytes[4..]).expect("recorder JSON is utf-8");
    parse_recorder_event_json(payload).expect("parse recorder event JSON")
}

fn read_or_update_golden(path: &Path, actual: &[u8]) -> Vec<u8> {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create recorder event golden dir");
        }
        fs::write(path, actual).expect("write recorder event golden");
        return actual.to_vec();
    }

    fs::read(path).unwrap_or_else(|err| {
        panic!(
            "missing recorder event golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test recorder_event_golden",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, event: &RecorderEvent) {
    let actual = encode_append_log_record(event);
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual);
    if expected != actual {
        let actual_path = path.with_extension("actual.bin");
        let _ = fs::write(&actual_path, &actual);
        panic!(
            "recorder event byte golden drift detected for {name}. Review the byte diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test recorder_event_golden",
            path.display(),
            actual_path.display()
        );
    }

    let decoded = decode_append_log_record(&expected);
    assert_eq!(
        decoded, *event,
        "{name} golden must decode to fixture event"
    );
}

#[test]
fn recorder_event_append_log_bytes_match_goldens() {
    for (name, event) in recorder_event_fixtures() {
        assert_matches_golden(name, &event);
    }
}
