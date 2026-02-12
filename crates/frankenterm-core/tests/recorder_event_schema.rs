use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use frankenterm_core::recording::{
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
    RecorderLifecyclePhase, RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    parse_recorder_event_json,
};
use serde_json::{Value, json};

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn recorder_schema_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("flight-recorder")
        .join("ft-recorder-event-v1.json")
}

fn load_schema() -> Value {
    let path = recorder_schema_path();
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("invalid JSON in {}: {e}", path.display()))
}

fn sample_event(payload: RecorderEventPayload) -> RecorderEvent {
    RecorderEvent {
        schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
        event_id: "evt_01JSCHEMA".to_string(),
        pane_id: 17,
        session_id: Some("sess_01JSCHEMA".to_string()),
        workflow_id: Some("wf_01JSCHEMA".to_string()),
        correlation_id: Some("corr_01JSCHEMA".to_string()),
        source: RecorderEventSource::WorkflowEngine,
        occurred_at_ms: 1_739_396_500_000,
        recorded_at_ms: 1_739_396_500_111,
        sequence: 9,
        causality: RecorderEventCausality {
            parent_event_id: Some("evt_parent".to_string()),
            trigger_event_id: Some("evt_trigger".to_string()),
            root_event_id: Some("evt_root".to_string()),
        },
        payload,
    }
}

#[test]
fn recorder_schema_has_required_contract_fields() {
    let schema = load_schema();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["title"], "FT Recorder Event v1");
    assert_eq!(schema["type"], "object");

    let required = schema["required"]
        .as_array()
        .expect("schema.required must be an array");
    let required_set: HashSet<&str> = required.iter().filter_map(Value::as_str).collect();

    for key in [
        "schema_version",
        "event_id",
        "pane_id",
        "session_id",
        "workflow_id",
        "correlation_id",
        "source",
        "occurred_at_ms",
        "recorded_at_ms",
        "sequence",
        "causality",
        "event_type",
    ] {
        assert!(required_set.contains(key), "missing required key: {key}");
    }
}

#[test]
fn recorder_schema_declares_all_event_types() {
    let schema = load_schema();
    let event_type_values = schema["properties"]["event_type"]["enum"]
        .as_array()
        .expect("event_type enum must be an array");
    let declared: HashSet<&str> = event_type_values.iter().filter_map(Value::as_str).collect();
    let expected: HashSet<&str> = HashSet::from([
        "ingress_text",
        "egress_output",
        "control_marker",
        "lifecycle_marker",
    ]);
    assert_eq!(declared, expected);
}

#[test]
fn recorder_event_roundtrips_all_variants() {
    let events = [
        sample_event(RecorderEventPayload::IngressText {
            text: "ft robot send 17 /compact".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::Partial,
            ingress_kind: RecorderIngressKind::SendText,
        }),
        sample_event(RecorderEventPayload::EgressOutput {
            text: "Compacting conversation...".to_string(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        }),
        sample_event(RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::PolicyDecision,
            details: json!({ "decision": "require_approval", "policy_id": "P-17" }),
        }),
        sample_event(RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: RecorderLifecyclePhase::CaptureStarted,
            reason: Some("daemon_startup".to_string()),
            details: json!({ "host": "local", "mux": "wezterm" }),
        }),
    ];

    for event in events {
        let encoded = serde_json::to_string(&event).expect("serialize event");
        let decoded = parse_recorder_event_json(&encoded).expect("parse event");
        assert_eq!(decoded, event);
    }
}

#[test]
fn recorder_event_additive_fields_are_compatible_within_v1() {
    let mut value = serde_json::to_value(sample_event(RecorderEventPayload::EgressOutput {
        text: "line".to_string(),
        encoding: RecorderTextEncoding::Utf8,
        redaction: RecorderRedactionLevel::None,
        segment_kind: RecorderSegmentKind::Delta,
        is_gap: false,
    }))
    .expect("serialize to value");

    value["producer_build"] = json!("2026-02-12-nightly");
    value["details"] = json!({ "future_optional_key": "ok" });

    let encoded = serde_json::to_string(&value).expect("value to string");
    let decoded = parse_recorder_event_json(&encoded).expect("additive fields should parse");
    assert_eq!(decoded.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
}

#[test]
fn recorder_event_rejects_unknown_schema_version() {
    let mut value = serde_json::to_value(sample_event(RecorderEventPayload::ControlMarker {
        control_marker_type: RecorderControlMarkerType::PromptBoundary,
        details: json!({ "boundary": "osc133" }),
    }))
    .expect("serialize to value");
    value["schema_version"] = json!("ft.recorder.event.v2");

    let encoded = serde_json::to_string(&value).expect("value to string");
    let err = parse_recorder_event_json(&encoded).expect_err("unknown schema version must fail");
    let msg = err.to_string();
    assert!(msg.contains("unsupported recorder event schema version"));
}
