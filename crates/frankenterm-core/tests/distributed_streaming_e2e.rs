#![cfg(feature = "distributed")]

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use frankenterm_core::distributed::validate_token;
use frankenterm_core::patterns::{AgentType, Severity};
use frankenterm_core::storage::{EventQuery, PaneRecord, StorageHandle, StoredEvent};
use frankenterm_core::wire_protocol::{
    Aggregator, DetectionNotice, IngestResult, PaneDelta, PaneMeta, WireEnvelope, WirePayload,
};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn emit_artifact(label: &str, value: serde_json::Value) {
    eprintln!("[ARTIFACT][distributed-streaming-e2e] {label}={value}");
}

#[derive(Default)]
struct BridgeDiagnostics {
    duplicates: usize,
    pane_reorder_drops: usize,
    pane_seq_gap_repairs: usize,
    replay_event_codes: Vec<String>,
}

struct DistributedBridge {
    aggregator: Aggregator,
    storage: StorageHandle,
    pane_seq_by_pane: HashMap<u64, u64>,
    diagnostics: BridgeDiagnostics,
}

impl DistributedBridge {
    async fn new(db_path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_capacity(db_path, 32).await
    }

    async fn new_with_capacity(
        db_path: &str,
        max_agents: usize,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            aggregator: Aggregator::new(max_agents),
            storage: StorageHandle::new(db_path).await?,
            pane_seq_by_pane: HashMap::new(),
            diagnostics: BridgeDiagnostics::default(),
        })
    }

    async fn ingest_envelope(
        &mut self,
        envelope: WireEnvelope,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let raw = envelope.to_json()?;
        self.ingest_raw(&raw).await
    }

    async fn ingest_raw(&mut self, raw: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        match self.aggregator.ingest(&raw)? {
            IngestResult::Accepted(payload) => self.persist_payload(payload).await,
            IngestResult::Duplicate { sender: _, seq: _ } => {
                self.diagnostics.duplicates += 1;
                Ok(())
            }
        }
    }

    async fn persist_payload(
        &mut self,
        payload: WirePayload,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match payload {
            WirePayload::PaneMeta(meta) => {
                self.upsert_pane_meta(meta).await?;
            }
            WirePayload::PaneDelta(delta) => {
                self.persist_delta(delta).await?;
            }
            WirePayload::Gap(gap) => {
                let reason = format!(
                    "distributed_gap:{}:{}:{}",
                    gap.reason, gap.seq_before, gap.seq_after
                );
                let _ = self.storage.record_gap(gap.pane_id, &reason).await?;
            }
            WirePayload::Detection(detection) => {
                self.persist_detection(detection).await?;
            }
            WirePayload::PanesMeta(panes) => {
                for pane in panes.panes {
                    self.upsert_pane_meta(pane).await?;
                }
            }
        }

        Ok(())
    }

    async fn upsert_pane_meta(&self, meta: PaneMeta) -> Result<(), Box<dyn std::error::Error>> {
        let pane = PaneRecord {
            pane_id: meta.pane_id,
            pane_uuid: meta.pane_uuid,
            domain: meta.domain,
            window_id: None,
            tab_id: None,
            title: meta.title,
            cwd: meta.cwd,
            tty_name: None,
            first_seen_at: meta.timestamp_ms,
            last_seen_at: meta.timestamp_ms,
            observed: meta.observed,
            ignore_reason: None,
            last_decision_at: Some(meta.timestamp_ms),
        };
        self.storage.upsert_pane(pane).await?;
        Ok(())
    }

    async fn ensure_pane_exists(&self, pane_id: u64) -> Result<(), Box<dyn std::error::Error>> {
        if self.storage.get_pane(pane_id).await?.is_some() {
            return Ok(());
        }

        let ts = now_ms();
        self.storage
            .upsert_pane(PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "distributed".to_string(),
                window_id: None,
                tab_id: None,
                title: Some(format!("remote-pane-{pane_id}")),
                cwd: Some("/remote".to_string()),
                tty_name: None,
                first_seen_at: ts,
                last_seen_at: ts,
                observed: true,
                ignore_reason: None,
                last_decision_at: Some(ts),
            })
            .await?;
        Ok(())
    }

    async fn persist_delta(&mut self, delta: PaneDelta) -> Result<(), Box<dyn std::error::Error>> {
        self.ensure_pane_exists(delta.pane_id).await?;

        let expected = self
            .pane_seq_by_pane
            .get(&delta.pane_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);

        if delta.seq < expected {
            // Out-of-order/duplicate at pane stream level: record deterministic diagnostic gap.
            let reason = format!(
                "distributed_out_of_order:expected={expected}:actual={}",
                delta.seq
            );
            let _ = self.storage.record_gap(delta.pane_id, &reason).await?;
            self.diagnostics.pane_reorder_drops += 1;
            self.diagnostics
                .replay_event_codes
                .push("dist.replay_detected".to_string());
            return Ok(());
        }

        if delta.seq > expected {
            // Discontinuity in remote pane sequence: preserve it as explicit gap before persisting.
            let reason = format!(
                "distributed_seq_gap:expected={expected}:actual={}",
                delta.seq
            );
            let _ = self.storage.record_gap(delta.pane_id, &reason).await?;
            self.diagnostics.pane_seq_gap_repairs += 1;
        }

        let _ = self
            .storage
            .append_segment(
                delta.pane_id,
                &delta.content,
                Some(format!("remote_seq:{}", delta.seq)),
            )
            .await?;
        self.pane_seq_by_pane.insert(delta.pane_id, delta.seq);

        Ok(())
    }

    async fn persist_detection(
        &self,
        detection: DetectionNotice,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rule_id = detection.rule_id.clone();
        let event = StoredEvent {
            id: 0,
            pane_id: detection.pane_id,
            rule_id: rule_id.clone(),
            agent_type: detection.agent_type.to_string(),
            event_type: detection.event_type,
            severity: severity_label(detection.severity).to_string(),
            confidence: detection.confidence,
            extracted: Some(detection.extracted),
            matched_text: Some(detection.matched_text),
            segment_id: None,
            detected_at: detection.detected_at_ms,
            dedupe_key: Some(format!(
                "{}:{}:{}",
                detection.pane_id, rule_id, detection.detected_at_ms
            )),
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        };

        let _ = self.storage.record_event(event).await?;
        Ok(())
    }
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Critical => "critical",
    }
}

fn pane_meta(pane_id: u64) -> PaneMeta {
    PaneMeta {
        pane_id,
        pane_uuid: Some(format!("remote-{pane_id}")),
        domain: "agent-swarm".to_string(),
        title: Some(format!("agent-{pane_id}")),
        cwd: Some("/swarm/project".to_string()),
        rows: Some(24),
        cols: Some(120),
        observed: true,
        timestamp_ms: now_ms(),
    }
}

fn pane_delta(pane_id: u64, seq: u64, content: &str) -> PaneDelta {
    PaneDelta {
        pane_id,
        seq,
        content: content.to_string(),
        content_len: content.len(),
        captured_at_ms: now_ms(),
    }
}

fn detection_notice(pane_id: u64) -> DetectionNotice {
    DetectionNotice {
        rule_id: "codex.usage.reached".to_string(),
        agent_type: AgentType::Codex,
        event_type: "usage_reached".to_string(),
        severity: Severity::Critical,
        confidence: 0.99,
        extracted: serde_json::json!({"reset_time":"2026-02-13T23:00:00Z"}),
        matched_text: "usage threshold reached".to_string(),
        pane_id,
        pane_uuid: Some(format!("remote-{pane_id}")),
        detected_at_ms: now_ms(),
    }
}

#[tokio::test]
async fn distributed_streaming_e2e_happy_path_persists_and_is_queryable() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("distributed_streaming.db");
    let mut bridge = DistributedBridge::new(db_path.to_str().expect("db path"))
        .await
        .expect("bridge");

    let sender = "agent-alpha";
    bridge
        .ingest_envelope(WireEnvelope::new(
            1,
            sender,
            WirePayload::PaneMeta(pane_meta(7)),
        ))
        .await
        .expect("pane meta");
    bridge
        .ingest_envelope(WireEnvelope::new(
            2,
            sender,
            WirePayload::PaneDelta(pane_delta(7, 1, "DIST_STREAM_MARKER line one")),
        ))
        .await
        .expect("delta 1");
    bridge
        .ingest_envelope(WireEnvelope::new(
            3,
            sender,
            WirePayload::PaneDelta(pane_delta(7, 2, "DIST_STREAM_MARKER line two")),
        ))
        .await
        .expect("delta 2");
    bridge
        .ingest_envelope(WireEnvelope::new(
            4,
            sender,
            WirePayload::Detection(detection_notice(7)),
        ))
        .await
        .expect("detection");

    let panes = bridge.storage.get_panes().await.expect("panes");
    let hits = bridge
        .storage
        .search("DIST_STREAM_MARKER")
        .await
        .expect("search");
    let events = bridge
        .storage
        .get_events(EventQuery {
            pane_id: Some(7),
            ..EventQuery::default()
        })
        .await
        .expect("events");

    assert!(panes.iter().any(|pane| pane.pane_id == 7));
    assert_eq!(hits.len(), 2);
    assert_eq!(events.len(), 1);

    emit_artifact(
        "agent_log",
        serde_json::json!({
            "sender": sender,
            "messages_sent": 4,
            "pane_id": 7
        }),
    );
    emit_artifact(
        "aggregator_log",
        serde_json::json!({
            "accepted": bridge.aggregator.total_accepted(),
            "duplicates": bridge.diagnostics.duplicates,
            "tracked_agents": bridge.aggregator.agent_count(),
            "pane_reorder_drops": bridge.diagnostics.pane_reorder_drops,
            "pane_seq_gap_repairs": bridge.diagnostics.pane_seq_gap_repairs
        }),
    );
    let db_size = std::fs::metadata(&db_path).expect("db metadata").len();
    emit_artifact(
        "db_snapshot",
        serde_json::json!({
            "path": db_path.display().to_string(),
            "size_bytes": db_size,
            "pane_count": panes.len(),
            "segment_count": hits.len(),
            "event_count": events.len()
        }),
    );
    emit_artifact(
        "query_visibility",
        serde_json::json!({
            "robot_equivalent": {
                "state_panes": panes.len(),
                "search_hits": hits.len(),
                "events": events.len()
            }
        }),
    );
}

#[tokio::test]
async fn distributed_streaming_e2e_preserves_gap_and_handles_duplicate_out_of_order() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("distributed_streaming_robustness.db");
    let mut bridge = DistributedBridge::new(db_path.to_str().expect("db path"))
        .await
        .expect("bridge");

    let sender = "agent-beta";
    bridge
        .ingest_envelope(WireEnvelope::new(
            1,
            sender,
            WirePayload::PaneMeta(pane_meta(9)),
        ))
        .await
        .expect("meta");
    bridge
        .ingest_envelope(WireEnvelope::new(
            2,
            sender,
            WirePayload::PaneDelta(pane_delta(9, 1, "ROBUST_MARKER first")),
        ))
        .await
        .expect("delta1");
    bridge
        .ingest_envelope(WireEnvelope::new(
            3,
            sender,
            WirePayload::Gap(frankenterm_core::wire_protocol::GapNotice {
                pane_id: 9,
                seq_before: 1,
                seq_after: 3,
                reason: "upstream_disconnect".to_string(),
                detected_at_ms: now_ms(),
            }),
        ))
        .await
        .expect("explicit gap");
    bridge
        .ingest_envelope(WireEnvelope::new(
            4,
            sender,
            WirePayload::PaneDelta(pane_delta(9, 3, "ROBUST_MARKER after gap")),
        ))
        .await
        .expect("delta gap repair");

    // Duplicate at sender sequence layer (should be dropped by aggregator).
    bridge
        .ingest_envelope(WireEnvelope::new(
            4,
            sender,
            WirePayload::PaneDelta(pane_delta(9, 3, "ROBUST_MARKER duplicate sender seq")),
        ))
        .await
        .expect("sender duplicate");

    // Out-of-order pane sequence with newer sender sequence (gap + diagnostic expected).
    bridge
        .ingest_envelope(WireEnvelope::new(
            5,
            sender,
            WirePayload::PaneDelta(pane_delta(9, 2, "ROBUST_MARKER out of order")),
        ))
        .await
        .expect("pane out-of-order");

    let segments = bridge.storage.get_segments(9, 20).await.expect("segments");
    let gaps = bridge.storage.get_gaps().await.expect("gaps");
    let search_hits = bridge
        .storage
        .search("ROBUST_MARKER")
        .await
        .expect("search");

    assert_eq!(segments.len(), 2, "only canonical segments should persist");
    assert_eq!(
        search_hits.len(),
        2,
        "search should reflect deduped persistence"
    );
    assert!(
        gaps.iter()
            .any(|gap| gap.reason.contains("distributed_gap")),
        "explicit remote gap must be preserved"
    );
    assert!(
        gaps.iter()
            .any(|gap| gap.reason.contains("distributed_seq_gap")
                || gap.reason.contains("distributed_out_of_order")),
        "out-of-order/discontinuity should be represented as gap diagnostics"
    );
    assert_eq!(bridge.diagnostics.duplicates, 1);
    assert_eq!(bridge.diagnostics.pane_reorder_drops, 1);
    assert_eq!(bridge.diagnostics.pane_seq_gap_repairs, 1);
    assert_eq!(
        bridge.diagnostics.replay_event_codes,
        vec!["dist.replay_detected".to_string()],
        "out-of-order payload should emit deterministic replay code"
    );

    emit_artifact(
        "aggregator_log",
        serde_json::json!({
            "sender": sender,
            "accepted": bridge.aggregator.total_accepted(),
            "duplicates": bridge.diagnostics.duplicates,
            "pane_reorder_drops": bridge.diagnostics.pane_reorder_drops,
            "pane_seq_gap_repairs": bridge.diagnostics.pane_seq_gap_repairs,
            "stable_error_code": "dist.replay_detected"
        }),
    );
    emit_artifact(
        "agent_log",
        serde_json::json!({
            "sender": sender,
            "sequence_plan": [1,2,3,4,4,5],
            "pane_seq_plan": [1,1,3,3,3,2],
            "result": "robustness_validated"
        }),
    );
    let db_size = std::fs::metadata(&db_path).expect("db metadata").len();
    emit_artifact(
        "db_snapshot",
        serde_json::json!({
            "path": db_path.display().to_string(),
            "size_bytes": db_size,
            "segments": segments.len(),
            "gaps": gaps.len(),
            "search_hits": search_hits.len()
        }),
    );
}

#[tokio::test]
async fn distributed_streaming_e2e_rejects_malformed_wire_without_persisting() {
    use frankenterm_core::wire_protocol::WireProtocolError;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("distributed_streaming_malformed.db");
    let mut bridge = DistributedBridge::new(db_path.to_str().expect("db path"))
        .await
        .expect("bridge");

    let malformed = br#"{"seq":"oops","payload":{"type":"gap"}}"#;
    let err = bridge
        .ingest_raw(malformed)
        .await
        .expect_err("malformed payload should fail with structured error");
    let wire_err = err
        .downcast_ref::<WireProtocolError>()
        .expect("expected WireProtocolError");
    assert!(
        matches!(wire_err, WireProtocolError::InvalidJson(_)),
        "expected InvalidJson error for malformed wire payload"
    );
    assert_eq!(bridge.aggregator.total_rejected(), 1);
    assert_eq!(bridge.aggregator.total_accepted(), 0);

    let panes = bridge.storage.get_panes().await.expect("panes");
    let hits = bridge
        .storage
        .search("MALFORMED_MARKER")
        .await
        .expect("search");
    let gaps = bridge.storage.get_gaps().await.expect("gaps");
    let events = bridge
        .storage
        .get_events(EventQuery::default())
        .await
        .expect("events");

    assert!(
        panes.is_empty(),
        "malformed payload should not create panes"
    );
    assert!(
        hits.is_empty(),
        "malformed payload should not persist searchable segments"
    );
    assert!(gaps.is_empty(), "malformed payload should not persist gaps");
    assert!(
        events.is_empty(),
        "malformed payload should not persist events"
    );
}

#[tokio::test]
async fn distributed_streaming_e2e_enforces_agent_capacity_without_cross_sender_persist() {
    use frankenterm_core::wire_protocol::WireProtocolError;

    let temp_dir = tempfile::tempdir().expect("tempdir");
    let db_path = temp_dir.path().join("distributed_streaming_capacity.db");
    let mut bridge = DistributedBridge::new_with_capacity(db_path.to_str().expect("db path"), 1)
        .await
        .expect("bridge");

    bridge
        .ingest_envelope(WireEnvelope::new(
            1,
            "agent-cap-a",
            WirePayload::PaneMeta(pane_meta(41)),
        ))
        .await
        .expect("first sender should be accepted");

    let err = bridge
        .ingest_envelope(WireEnvelope::new(
            1,
            "agent-cap-b",
            WirePayload::PaneMeta(pane_meta(42)),
        ))
        .await
        .expect_err("second sender should be rejected at capacity");
    let wire_err = err
        .downcast_ref::<WireProtocolError>()
        .expect("expected WireProtocolError");
    assert!(matches!(
        wire_err,
        WireProtocolError::TooManyAgents { max: 1, sender: _ }
    ));
    assert_eq!(bridge.aggregator.total_rejected(), 1);
    assert_eq!(bridge.aggregator.total_accepted(), 1);

    let panes = bridge.storage.get_panes().await.expect("panes");
    assert_eq!(
        panes.len(),
        1,
        "rejected sender metadata must not persist to storage"
    );
    assert_eq!(panes[0].pane_id, 41);
}

#[test]
fn distributed_streaming_e2e_auth_missing_or_invalid_token_rejected_and_redacted() {
    use frankenterm_core::config::DistributedAuthMode;
    use frankenterm_core::distributed::DistributedSecurityError;

    let missing = validate_token(
        DistributedAuthMode::Token,
        Some("agent-a:expected-secret"),
        None,
        Some("agent-a"),
    )
    .expect_err("missing token should fail");
    assert_eq!(missing, DistributedSecurityError::MissingToken);
    assert_eq!(missing.code(), "dist.auth_failed");

    let invalid = validate_token(
        DistributedAuthMode::Token,
        Some("agent-a:expected-secret"),
        Some("agent-a:wrong-secret"),
        Some("agent-a"),
    )
    .expect_err("invalid token should fail");
    assert_eq!(invalid, DistributedSecurityError::AuthFailed);
    assert_eq!(invalid.code(), "dist.auth_failed");

    let missing_msg = missing.to_string();
    let invalid_msg = invalid.to_string();
    assert!(!missing_msg.contains("expected-secret"));
    assert!(!invalid_msg.contains("expected-secret"));
    assert!(!invalid_msg.contains("wrong-secret"));

    emit_artifact(
        "security_log",
        serde_json::json!({
            "auth_mode": "token",
            "missing_token_error_code": missing.code(),
            "invalid_token_error_code": invalid.code(),
            "redacted": true
        }),
    );
}
