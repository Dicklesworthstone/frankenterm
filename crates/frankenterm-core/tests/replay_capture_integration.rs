use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use frankenterm_core::recording::RecorderEventPayload;
use frankenterm_core::replay_fixture_harvest::{
    ArtifactReader, ArtifactWriterConfig, FixtureHarvester, FtreplayValidator, FtreplayWriter,
    HarvestSource,
};
use tempfile::tempdir;

fn write_egress_event(
    file: &mut File,
    event_id: &str,
    pane_id: u64,
    sequence: u64,
    text: &str,
) -> std::io::Result<()> {
    let event = serde_json::json!({
        "schema_version": "ft.recorder.event.v1",
        "event_id": event_id,
        "pane_id": pane_id,
        "session_id": format!("sess-{pane_id}"),
        "workflow_id": serde_json::Value::Null,
        "correlation_id": serde_json::Value::Null,
        "source": "wezterm_mux",
        "occurred_at_ms": 1_700_000_000_000u64 + sequence,
        "recorded_at_ms": 1_700_000_000_000u64 + sequence,
        "sequence": sequence,
        "causality": {
            "parent_event_id": serde_json::Value::Null,
            "trigger_event_id": serde_json::Value::Null,
            "root_event_id": serde_json::Value::Null
        },
        "event_type": "egress_output",
        "text": text,
        "encoding": "utf8",
        "redaction": "none",
        "segment_kind": "delta",
        "is_gap": false
    });
    writeln!(file, "{event}")
}

fn write_decision_event(file: &mut File, event_id: &str, sequence: u64) -> std::io::Result<()> {
    let event = serde_json::json!({
        "schema_version": "ft.recorder.event.v1",
        "event_id": event_id,
        "pane_id": 1,
        "session_id": "sess-incident",
        "workflow_id": "wf-1",
        "correlation_id": "corr-1",
        "source": "workflow_engine",
        "occurred_at_ms": 1_700_000_001_000u64 + sequence,
        "recorded_at_ms": 1_700_000_001_000u64 + sequence,
        "sequence": sequence,
        "causality": {
            "parent_event_id": serde_json::Value::Null,
            "trigger_event_id": serde_json::Value::Null,
            "root_event_id": serde_json::Value::Null
        },
        "event_type": "control_marker",
        "control_marker_type": "policy_decision",
        "details": {
            "decision": "allow",
            "reason": "integration",
            "rule_id": "policy.default.allow_non_alt"
        }
    });
    writeln!(file, "{event}")
}

fn write_fixture(path: &Path, include_secret: bool) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    write_egress_event(&mut file, "evt-1", 1, 0, "compile start")?;
    if include_secret {
        write_egress_event(
            &mut file,
            "evt-2",
            1,
            1,
            "Authorization: Bearer sk-test-secret-token-123456",
        )?;
    } else {
        write_egress_event(&mut file, "evt-2", 1, 1, "compile done")?;
    }
    write_decision_event(&mut file, "evt-3", 2)?;
    Ok(())
}

fn write_large_fixture(path: &Path, event_count: usize) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    for i in 0..event_count {
        write_egress_event(
            &mut file,
            &format!("evt-{i}"),
            (i % 4) as u64 + 1,
            i as u64,
            &format!("line-{i}"),
        )?;
    }
    write_decision_event(&mut file, "evt-decision", event_count as u64)?;
    Ok(())
}

fn rewrite_schema_version(path: &Path, from: &str, to: &str) -> std::io::Result<()> {
    let content = fs::read_to_string(path)?;
    fs::write(path, content.replace(from, to))
}

#[test]
fn replay_capture_roundtrip_preserves_decision_provenance() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture.jsonl");
    let artifact_path = tmp.path().join("capture.ftreplay");
    write_fixture(&source, false).expect("fixture");

    let harvester = FixtureHarvester::default();
    let artifact = harvester
        .harvest(HarvestSource::ManualExport(source.clone()))
        .expect("harvest artifact");

    let writer = FtreplayWriter::default();
    writer
        .write(&artifact, &artifact_path)
        .expect("write ftreplay artifact");

    let loaded = ArtifactReader::default()
        .open(&artifact_path)
        .expect("open artifact");
    assert_eq!(loaded.events.len(), artifact.events.len());

    let has_policy_decision = loaded.events.iter().any(|event| {
        matches!(
            &event.payload,
            RecorderEventPayload::ControlMarker {
                control_marker_type:
                    frankenterm_core::recording::RecorderControlMarkerType::PolicyDecision,
                ..
            }
        )
    });
    assert!(
        has_policy_decision,
        "expected decision provenance control marker"
    );

    let report = FtreplayValidator::validate_file(&artifact_path).expect("validate artifact");
    assert_eq!(report.event_count, loaded.events.len());
    assert!(report.merge_order_verified);
    assert!(report.sequence_monotonicity_verified);
}

#[test]
fn replay_capture_harvest_redacts_sensitive_text_before_write() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_sensitive.jsonl");
    let artifact_path = tmp.path().join("capture_sensitive.ftreplay");
    write_fixture(&source, true).expect("fixture");

    let harvester = FixtureHarvester::default();
    let artifact = harvester
        .harvest(HarvestSource::ManualExport(source.clone()))
        .expect("harvest artifact");

    assert!(artifact.events.iter().all(|event| match &event.payload {
        RecorderEventPayload::IngressText { text, .. }
        | RecorderEventPayload::EgressOutput { text, .. } => {
            !text.contains("sk-test-secret-token-123456")
        }
        _ => true,
    }));

    let writer = FtreplayWriter::default();
    writer
        .write(&artifact, &artifact_path)
        .expect("write artifact");
    let loaded = ArtifactReader::default()
        .open(&artifact_path)
        .expect("open artifact");

    assert!(loaded.events.iter().all(|event| match &event.payload {
        RecorderEventPayload::IngressText { text, .. }
        | RecorderEventPayload::EgressOutput { text, .. } => {
            !text.contains("sk-test-secret-token-123456")
        }
        _ => true,
    }));
}

#[test]
fn replay_capture_streaming_reader_handles_small_pipeline_fixture() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_stream.jsonl");
    let artifact_path = tmp.path().join("capture_stream.ftreplay");
    write_fixture(&source, false).expect("fixture");

    let harvester = FixtureHarvester::default();
    let artifact = harvester
        .harvest(HarvestSource::ManualExport(source))
        .expect("harvest artifact");
    FtreplayWriter::default()
        .write(&artifact, &artifact_path)
        .expect("write artifact");

    let reader = ArtifactReader::default();
    let mut stream = reader.stream_events(&artifact_path).expect("stream");
    let mut count = 0usize;
    while let Some(event) = stream.next() {
        let _ = event.expect("stream item");
        count += 1;
    }

    assert_eq!(count, artifact.events.len());
    assert!(count >= 3);
}

#[test]
fn replay_capture_reader_migrates_v0_artifact_to_current_schema() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_migration.jsonl");
    let artifact_path = tmp.path().join("capture_migration.ftreplay");
    write_fixture(&source, false).expect("fixture");

    let artifact = FixtureHarvester::default()
        .harvest(HarvestSource::ManualExport(source))
        .expect("harvest artifact");
    FtreplayWriter::default()
        .write(&artifact, &artifact_path)
        .expect("write artifact");

    rewrite_schema_version(&artifact_path, "ftreplay.v1", "ftreplay.v0")
        .expect("rewrite schema version");

    let loaded = ArtifactReader::default()
        .open(&artifact_path)
        .expect("open migrated artifact");
    assert_eq!(loaded.schema_version, "ftreplay.v1");
    assert!(loaded.migration_report.is_some());
}

#[test]
fn replay_capture_streaming_reader_handles_large_pipeline_fixture() {
    const EVENT_COUNT: usize = 100_000;

    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_large_stream.jsonl");
    let artifact_path = tmp.path().join("capture_large_stream.ftreplay");
    write_large_fixture(&source, EVENT_COUNT).expect("large fixture");

    let artifact = FixtureHarvester::default()
        .harvest(HarvestSource::ManualExport(source))
        .expect("harvest artifact");
    FtreplayWriter::with_config(ArtifactWriterConfig {
        max_events_per_chunk: EVENT_COUNT + 1_000,
    })
    .write(&artifact, &artifact_path)
    .expect("write artifact");

    let mut count = 0usize;
    let mut stream = ArtifactReader::default()
        .stream_events(&artifact_path)
        .expect("stream events");
    while let Some(event) = stream.next() {
        let _ = event.expect("stream item");
        count += 1;
    }

    assert_eq!(count, artifact.events.len());
    assert!(count > EVENT_COUNT);
}
