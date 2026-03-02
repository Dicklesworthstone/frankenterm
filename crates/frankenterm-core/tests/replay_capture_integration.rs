use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::Utc;
use frankenterm_core::recording::RecorderEventPayload;
use frankenterm_core::replay_fixture_harvest::{
    ArtifactReader, ArtifactWriterConfig, FixtureHarvester, FtreplayValidator, FtreplayWriter,
    HarvestSource,
};
use serde_json::{json, Value};
use tempfile::tempdir;

#[derive(Debug, Default, Clone)]
struct CaptureMetrics {
    capture_events: u64,
    decisions_captured: u64,
    secrets_detected: u64,
    secrets_redacted: u64,
    compression_ratio: Option<f64>,
    read_events: u64,
    roundtrip_match: bool,
}

struct CaptureLogGuard {
    test_name: &'static str,
    scenario_id: &'static str,
    decision_path: &'static str,
    correlation_id: String,
    inputs: Value,
    artifact_path: Option<String>,
    metrics: CaptureMetrics,
}

impl CaptureLogGuard {
    fn new(
        test_name: &'static str,
        scenario_id: &'static str,
        decision_path: &'static str,
        inputs: Value,
    ) -> Self {
        let correlation_id = format!("{test_name}-{}", Utc::now().format("%Y%m%dT%H%M%S%.f"));
        Self {
            test_name,
            scenario_id,
            decision_path,
            correlation_id,
            inputs,
            artifact_path: None,
            metrics: CaptureMetrics::default(),
        }
    }

    fn set_artifact_path(&mut self, path: &Path) {
        self.artifact_path = Some(path.to_string_lossy().into_owned());
    }

    fn set_metrics(&mut self, metrics: CaptureMetrics) {
        self.metrics = metrics;
    }
}

impl Drop for CaptureLogGuard {
    fn drop(&mut self) {
        let log_dir = capture_artifact_dir();
        if fs::create_dir_all(&log_dir).is_err() {
            return;
        }
        let log_path = log_dir.join(format!("{}.jsonl", self.test_name));
        let artifact_path = self
            .artifact_path
            .clone()
            .unwrap_or_else(|| log_path.to_string_lossy().into_owned());
        let panicking = std::thread::panicking();
        let status = if panicking { "failed" } else { "passed" };
        let outcome = if panicking { "failed" } else { "pass" };
        let reason_code = if panicking {
            json!("assertion_failed")
        } else {
            json!("completed")
        };
        let error_code = if panicking {
            json!("ASSERTION-FAILED")
        } else {
            Value::Null
        };

        let payload = json!({
            "timestamp": Utc::now().to_rfc3339(),
            "component": "replay_capture_integration",
            "run_id": self.test_name,
            "scenario_id": self.scenario_id,
            "pane_id": Value::Null,
            "step": "integration_test",
            "status": status,
            "correlation_id": self.correlation_id,
            "decision_path": self.decision_path,
            "inputs": self.inputs,
            "outcome": outcome,
            "reason_code": reason_code,
            "error_code": error_code,
            "capture_events": self.metrics.capture_events,
            "decisions_captured": self.metrics.decisions_captured,
            "secrets_detected": self.metrics.secrets_detected,
            "secrets_redacted": self.metrics.secrets_redacted,
            "compression_ratio": self.metrics.compression_ratio,
            "read_events": self.metrics.read_events,
            "roundtrip_match": self.metrics.roundtrip_match,
            "artifact_path": artifact_path,
        });

        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log_path) {
            let _ = writeln!(file, "{payload}");
        }
    }
}

fn capture_artifact_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-artifacts/capture")
}

fn compression_ratio(source_path: &Path, artifact_path: &Path) -> Option<f64> {
    let source_bytes = fs::metadata(source_path).ok()?.len();
    let artifact_bytes = fs::metadata(artifact_path).ok()?.len();
    if source_bytes == 0 || artifact_bytes == 0 {
        return None;
    }
    Some(source_bytes as f64 / artifact_bytes as f64)
}

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
    let mut capture_log = CaptureLogGuard::new(
        "replay_capture_roundtrip_preserves_decision_provenance",
        "i_06_full_capture_pipeline_roundtrip",
        "harvest_write_read_validate",
        json!({
            "fixture_source": "manual_export",
            "include_secret": false,
        }),
    );

    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture.jsonl");
    let artifact_path = tmp.path().join("capture.ftreplay");
    write_fixture(&source, false).expect("fixture");
    capture_log.set_artifact_path(&artifact_path);

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
    let roundtrip_match = loaded.events.len() == artifact.events.len();
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
    capture_log.set_metrics(CaptureMetrics {
        capture_events: artifact.events.len() as u64,
        decisions_captured: u64::from(has_policy_decision),
        secrets_detected: 0,
        secrets_redacted: 0,
        compression_ratio: compression_ratio(&source, &artifact_path),
        read_events: loaded.events.len() as u64,
        roundtrip_match,
    });
}

#[test]
fn replay_capture_harvest_redacts_sensitive_text_before_write() {
    let mut capture_log = CaptureLogGuard::new(
        "replay_capture_harvest_redacts_sensitive_text_before_write",
        "i_08_redaction_roundtrip",
        "harvest_redact_write_read_verify",
        json!({
            "fixture_source": "manual_export",
            "include_secret": true,
        }),
    );

    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_sensitive.jsonl");
    let artifact_path = tmp.path().join("capture_sensitive.ftreplay");
    write_fixture(&source, true).expect("fixture");
    capture_log.set_artifact_path(&artifact_path);

    let harvester = FixtureHarvester::default();
    let artifact = harvester
        .harvest(HarvestSource::ManualExport(source.clone()))
        .expect("harvest artifact");

    let harvested_secret_free = artifact.events.iter().all(|event| match &event.payload {
        RecorderEventPayload::IngressText { text, .. }
        | RecorderEventPayload::EgressOutput { text, .. } => {
            !text.contains("sk-test-secret-token-123456")
        }
        _ => true,
    });
    assert!(harvested_secret_free);

    let writer = FtreplayWriter::default();
    writer
        .write(&artifact, &artifact_path)
        .expect("write artifact");
    let loaded = ArtifactReader::default()
        .open(&artifact_path)
        .expect("open artifact");

    let loaded_secret_free = loaded.events.iter().all(|event| match &event.payload {
        RecorderEventPayload::IngressText { text, .. }
        | RecorderEventPayload::EgressOutput { text, .. } => {
            !text.contains("sk-test-secret-token-123456")
        }
        _ => true,
    });
    assert!(loaded_secret_free);

    capture_log.set_metrics(CaptureMetrics {
        capture_events: artifact.events.len() as u64,
        decisions_captured: 1,
        secrets_detected: 1,
        secrets_redacted: 1,
        compression_ratio: compression_ratio(&source, &artifact_path),
        read_events: loaded.events.len() as u64,
        roundtrip_match: harvested_secret_free && loaded_secret_free,
    });
}

#[test]
fn replay_capture_streaming_reader_handles_small_pipeline_fixture() {
    let mut capture_log = CaptureLogGuard::new(
        "replay_capture_streaming_reader_handles_small_pipeline_fixture",
        "i_06_streaming_read_small",
        "stream_events_verify_count",
        json!({
            "fixture_source": "manual_export",
            "event_volume": "small",
        }),
    );

    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_stream.jsonl");
    let artifact_path = tmp.path().join("capture_stream.ftreplay");
    write_fixture(&source, false).expect("fixture");
    capture_log.set_artifact_path(&artifact_path);

    let harvester = FixtureHarvester::default();
    let artifact = harvester
        .harvest(HarvestSource::ManualExport(source.clone()))
        .expect("harvest artifact");
    FtreplayWriter::default()
        .write(&artifact, &artifact_path)
        .expect("write artifact");

    let reader = ArtifactReader::default();
    let mut stream = reader.stream_events(&artifact_path).expect("stream");
    let mut count = 0usize;
    for event in &mut stream {
        let _ = event.expect("stream item");
        count += 1;
    }

    let roundtrip_match = count == artifact.events.len();
    assert_eq!(count, artifact.events.len());
    assert!(count >= 3);
    capture_log.set_metrics(CaptureMetrics {
        capture_events: artifact.events.len() as u64,
        decisions_captured: 1,
        secrets_detected: 0,
        secrets_redacted: 0,
        compression_ratio: compression_ratio(&source, &artifact_path),
        read_events: count as u64,
        roundtrip_match,
    });
}

#[test]
fn replay_capture_reader_migrates_v0_artifact_to_current_schema() {
    let mut capture_log = CaptureLogGuard::new(
        "replay_capture_reader_migrates_v0_artifact_to_current_schema",
        "i_09_schema_migration_v0_to_v1",
        "rewrite_schema_and_migrate",
        json!({
            "source_schema": "ftreplay.v0",
            "target_schema": "ftreplay.v1",
        }),
    );

    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_migration.jsonl");
    let artifact_path = tmp.path().join("capture_migration.ftreplay");
    write_fixture(&source, false).expect("fixture");
    capture_log.set_artifact_path(&artifact_path);

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
    let roundtrip_match = loaded.events.len() == artifact.events.len();
    assert_eq!(loaded.schema_version, "ftreplay.v1");
    assert!(loaded.migration_report.is_some());
    capture_log.set_metrics(CaptureMetrics {
        capture_events: artifact.events.len() as u64,
        decisions_captured: 1,
        secrets_detected: 0,
        secrets_redacted: 0,
        compression_ratio: compression_ratio(
            &tmp.path().join("capture_migration.jsonl"),
            &artifact_path,
        ),
        read_events: loaded.events.len() as u64,
        roundtrip_match,
    });
}

#[test]
fn replay_capture_streaming_reader_handles_large_pipeline_fixture() {
    const EVENT_COUNT: usize = 100_000;
    let mut capture_log = CaptureLogGuard::new(
        "replay_capture_streaming_reader_handles_large_pipeline_fixture",
        "i_10_large_artifact_streaming",
        "stream_events_verify_no_oom",
        json!({
            "fixture_source": "manual_export",
            "event_volume": EVENT_COUNT,
        }),
    );

    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("capture_large_stream.jsonl");
    let artifact_path = tmp.path().join("capture_large_stream.ftreplay");
    write_large_fixture(&source, EVENT_COUNT).expect("large fixture");
    capture_log.set_artifact_path(&artifact_path);

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
    for event in &mut stream {
        let _ = event.expect("stream item");
        count += 1;
    }

    let roundtrip_match = count == artifact.events.len();
    assert_eq!(count, artifact.events.len());
    assert!(count > EVENT_COUNT);
    capture_log.set_metrics(CaptureMetrics {
        capture_events: artifact.events.len() as u64,
        decisions_captured: 1,
        secrets_detected: 0,
        secrets_redacted: 0,
        compression_ratio: compression_ratio(
            &tmp.path().join("capture_large_stream.jsonl"),
            &artifact_path,
        ),
        read_events: count as u64,
        roundtrip_match,
    });
}
