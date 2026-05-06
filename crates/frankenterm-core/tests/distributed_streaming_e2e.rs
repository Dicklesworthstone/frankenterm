#![cfg(feature = "distributed")]

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use frankenterm_core::distributed::validate_token;
use frankenterm_core::patterns::{AgentType, Severity};
use frankenterm_core::storage::{EventQuery, PaneRecord, StorageHandle, StoredEvent};
use frankenterm_core::wire_protocol::{
    Aggregator, DEFAULT_AGENT_STALE_AFTER_MS, DetectionNotice, GapNotice, IngestResult, PaneDelta,
    PaneMeta, WireEnvelope, WirePayload, WireProtocolError,
};

fn run_async_test<F>(future: F)
where
    F: std::future::Future<Output = ()>,
{
    use frankenterm_core::runtime_async::CompatRuntime;
    let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime");
    runtime.block_on(future);
}

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
        Self::new_with_capacity_and_stale(db_path, max_agents, DEFAULT_AGENT_STALE_AFTER_MS).await
    }

    async fn new_with_capacity_and_stale(
        db_path: &str,
        max_agents: usize,
        stale_after_ms: i64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            aggregator: Aggregator::with_stale_after(max_agents, stale_after_ms),
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

    async fn ingest_envelope_at(
        &mut self,
        envelope: WireEnvelope,
        received_at_ms: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match self
            .aggregator
            .ingest_envelope_at(envelope, received_at_ms)?
        {
            IngestResult::Accepted(payload) => self.persist_payload(payload).await,
            IngestResult::Duplicate { sender: _, seq: _ } => {
                self.diagnostics.duplicates += 1;
                Ok(())
            }
        }
    }

    async fn ingest_raw(&mut self, raw: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
        match self.aggregator.ingest(raw)? {
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
                self.ensure_pane_exists(gap.pane_id).await?;
                self.pane_seq_by_pane
                    .entry(gap.pane_id)
                    .and_modify(|current| {
                        *current = (*current).max(gap.seq_after.saturating_sub(1));
                    })
                    .or_insert_with(|| gap.seq_after.saturating_sub(1));
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
            .map(|last_seen| last_seen.saturating_add(1))
            .unwrap_or(0);

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
            let reason = inferred_seq_gap_reason(expected, delta.seq);
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

fn distributed_pane_meta(pane_id: u64, sender: &str) -> PaneMeta {
    let mut meta = pane_meta(pane_id);
    meta.domain = format!("distributed:{sender}:prod");
    meta.title = Some(format!("{sender}-pane-{pane_id}"));
    meta
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

fn wire_protocol_error_code(error: &WireProtocolError) -> &'static str {
    match error {
        WireProtocolError::InvalidJson(_) => "dist.invalid_json",
        WireProtocolError::MessageTooLarge { .. } => "dist.message_too_large",
        WireProtocolError::VersionMismatch { .. } => "dist.version_mismatch",
        WireProtocolError::InvalidSender { .. } => "dist.invalid_sender",
        WireProtocolError::TooManyAgents { .. } => "dist.too_many_agents",
        WireProtocolError::InvalidSequence { .. } => "dist.invalid_sequence",
    }
}

fn fault_receipt(
    fault: &str,
    sender: &str,
    envelope_seq: u64,
    pane_seq: Option<u64>,
    received_at_ms: i64,
    decision: &str,
    error_code: Option<&str>,
    stale_pruned_senders: &[&str],
    bridge: &DistributedBridge,
) -> serde_json::Value {
    serde_json::json!({
        "fault": fault,
        "sender": sender,
        "envelope_seq": envelope_seq,
        "pane_seq": pane_seq,
        "received_at_ms": received_at_ms,
        "decision": decision,
        "error_code": error_code,
        "stale_pruned_senders": stale_pruned_senders,
        "accepted_total": bridge.aggregator.total_accepted(),
        "rejected_total": bridge.aggregator.total_rejected(),
        "dedup_total": bridge.diagnostics.duplicates,
        "pane_reorder_drops": bridge.diagnostics.pane_reorder_drops,
        "pane_seq_gap_repairs": bridge.diagnostics.pane_seq_gap_repairs,
        "tracked_agents": bridge.aggregator.agent_count(),
    })
}

fn inferred_gap_bounds(expected: u64, actual: u64) -> (u64, u64) {
    let seq_before = if expected == 0 {
        0
    } else {
        expected.saturating_sub(1)
    };
    (seq_before, actual)
}

fn inferred_seq_gap_reason(expected: u64, actual: u64) -> String {
    let (seq_before, seq_after) = inferred_gap_bounds(expected, actual);
    format!(
        "distributed_gap:distributed_seq_gap:expected={expected}:actual={actual}:{seq_before}:{seq_after}"
    )
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

#[test]
fn distributed_streaming_e2e_happy_path_persists_and_is_queryable() {
    run_async_test(async {
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
                WirePayload::PaneDelta(pane_delta(7, 0, "DIST_STREAM_MARKER line one")),
            ))
            .await
            .expect("delta 1");
        bridge
            .ingest_envelope(WireEnvelope::new(
                3,
                sender,
                WirePayload::PaneDelta(pane_delta(7, 1, "DIST_STREAM_MARKER line two")),
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
        let gaps = bridge.storage.get_gaps().await.expect("gaps");
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
        assert!(
            gaps.is_empty(),
            "happy-path first segment should not synthesize a distributed seq gap"
        );
        assert_eq!(events.len(), 1);
        assert_eq!(bridge.diagnostics.pane_seq_gap_repairs, 0);

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
    });
}

#[test]
fn distributed_streaming_e2e_preserves_gap_and_handles_duplicate_out_of_order() {
    run_async_test(async {
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
                WirePayload::PaneDelta(pane_delta(9, 0, "ROBUST_MARKER first")),
            ))
            .await
            .expect("delta1");
        bridge
            .ingest_envelope(WireEnvelope::new(
                3,
                sender,
                WirePayload::Gap(frankenterm_core::wire_protocol::GapNotice {
                    pane_id: 9,
                    seq_before: 0,
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
                .any(|gap| gap.reason.contains("distributed_out_of_order")),
            "out-of-order/discontinuity should be represented as gap diagnostics"
        );
        assert!(
            !gaps
                .iter()
                .any(|gap| gap.reason.contains("distributed_seq_gap")),
            "explicit remote gap should advance pane sequence tracking without adding a second synthetic seq gap"
        );
        assert_eq!(bridge.diagnostics.duplicates, 1);
        assert_eq!(bridge.diagnostics.pane_reorder_drops, 1);
        assert_eq!(bridge.diagnostics.pane_seq_gap_repairs, 0);
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
                "pane_seq_plan": [0,0,3,3,3,2],
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
    });
}

#[test]
fn distributed_streaming_e2e_lossy_stale_session_drill_preserves_visibility() {
    run_async_test(async {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("distributed_streaming_lossy_stale.db");
        let mut bridge = DistributedBridge::new_with_capacity_and_stale(
            db_path.to_str().expect("db path"),
            1,
            50,
        )
        .await
        .expect("bridge");
        let mut receipts = Vec::new();

        let sender_a = "agent-loss-a";
        let sender_b = "agent-loss-b";
        let sender_c = "agent-loss-c";

        let mut meta_a = WireEnvelope::new(
            1,
            sender_a,
            WirePayload::PaneMeta(distributed_pane_meta(71, sender_a)),
        );
        meta_a.sent_at_ms = 10_000;
        bridge
            .ingest_envelope_at(meta_a, 100)
            .await
            .expect("agent a metadata should be accepted");
        receipts.push(fault_receipt(
            "initial_meta",
            sender_a,
            1,
            None,
            100,
            "accepted",
            None,
            &[],
            &bridge,
        ));

        let mut first_delta = WireEnvelope::new(
            2,
            sender_a,
            WirePayload::PaneDelta(pane_delta(71, 0, "LOSSY_STALE_MARKER first")),
        );
        first_delta.sent_at_ms = 10_001;
        bridge
            .ingest_envelope_at(first_delta, 110)
            .await
            .expect("first delta should be accepted");
        receipts.push(fault_receipt(
            "first_delta",
            sender_a,
            2,
            Some(0),
            110,
            "accepted",
            None,
            &[],
            &bridge,
        ));

        let mut delayed_duplicate = WireEnvelope::new(
            2,
            sender_a,
            WirePayload::PaneDelta(pane_delta(71, 0, "LOSSY_STALE_MARKER duplicate")),
        );
        delayed_duplicate.sent_at_ms = 1;
        bridge
            .ingest_envelope_at(delayed_duplicate, 130)
            .await
            .expect("duplicate should refresh liveness without persisting");
        receipts.push(fault_receipt(
            "delayed_duplicate",
            sender_a,
            2,
            Some(0),
            130,
            "dedup",
            None,
            &[],
            &bridge,
        ));

        let mut rejected_recent_sender = WireEnvelope::new(
            1,
            sender_b,
            WirePayload::PaneMeta(distributed_pane_meta(72, sender_b)),
        );
        rejected_recent_sender.sent_at_ms = 2;
        let err = bridge
            .ingest_envelope_at(rejected_recent_sender, 160)
            .await
            .expect_err("recent refreshed sender should still occupy capacity");
        let wire_err = err
            .downcast_ref::<WireProtocolError>()
            .expect("capacity rejection should be a wire protocol error");
        assert!(matches!(
            wire_err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        receipts.push(fault_receipt(
            "recent_sender_capacity_reject",
            sender_b,
            1,
            None,
            160,
            "rejected",
            Some(wire_protocol_error_code(wire_err)),
            &[],
            &bridge,
        ));

        let mut replayed_pane_seq = WireEnvelope::new(
            3,
            sender_a,
            WirePayload::PaneDelta(pane_delta(71, 0, "LOSSY_STALE_MARKER replay")),
        );
        replayed_pane_seq.sent_at_ms = 10_002;
        bridge
            .ingest_envelope_at(replayed_pane_seq, 170)
            .await
            .expect("new sender sequence with replayed pane seq should be diagnosed");
        receipts.push(fault_receipt(
            "replayed_pane_seq",
            sender_a,
            3,
            Some(0),
            170,
            "accepted_replay_dropped",
            Some("dist.replay_detected"),
            &[],
            &bridge,
        ));

        let mut dropped_stream_gap = WireEnvelope::new(
            4,
            sender_a,
            WirePayload::Gap(GapNotice {
                pane_id: 71,
                seq_before: 0,
                seq_after: 3,
                reason: "dropped_stream_reconnect".to_string(),
                detected_at_ms: now_ms(),
            }),
        );
        dropped_stream_gap.sent_at_ms = 10_003;
        bridge
            .ingest_envelope_at(dropped_stream_gap, 175)
            .await
            .expect("explicit dropped-stream gap should persist");
        receipts.push(fault_receipt(
            "dropped_stream_gap",
            sender_a,
            4,
            None,
            175,
            "accepted",
            None,
            &[],
            &bridge,
        ));

        let mut after_gap = WireEnvelope::new(
            5,
            sender_a,
            WirePayload::PaneDelta(pane_delta(71, 3, "LOSSY_STALE_MARKER after gap")),
        );
        after_gap.sent_at_ms = 10_004;
        bridge
            .ingest_envelope_at(after_gap, 180)
            .await
            .expect("delta after explicit gap should persist");
        receipts.push(fault_receipt(
            "after_gap_delta",
            sender_a,
            5,
            Some(3),
            180,
            "accepted",
            None,
            &[],
            &bridge,
        ));

        assert_eq!(bridge.aggregator.agent_last_seq(sender_a), Some(5));
        let mut stale_replacement = WireEnvelope::new(
            1,
            sender_c,
            WirePayload::PaneMeta(distributed_pane_meta(73, sender_c)),
        );
        stale_replacement.sent_at_ms = 99_999;
        bridge
            .ingest_envelope_at(stale_replacement, 240)
            .await
            .expect("stale sender should be pruned before accepting replacement");
        let stale_pruned = if bridge.aggregator.agent_last_seq(sender_a).is_none() {
            vec![sender_a]
        } else {
            Vec::new()
        };
        assert_eq!(stale_pruned, vec![sender_a]);
        receipts.push(fault_receipt(
            "stale_prune_replacement",
            sender_c,
            1,
            None,
            240,
            "accepted",
            None,
            &stale_pruned,
            &bridge,
        ));

        let mut panes = bridge.storage.get_panes().await.expect("panes");
        panes.sort_by_key(|pane| pane.pane_id);
        let segments = bridge.storage.get_segments(71, 20).await.expect("segments");
        let gaps = bridge.storage.get_gaps().await.expect("gaps");
        let search_hits = bridge
            .storage
            .search("LOSSY_STALE_MARKER")
            .await
            .expect("search");

        assert_eq!(receipts.len(), 8);
        assert_eq!(bridge.aggregator.total_accepted(), 6);
        assert_eq!(bridge.aggregator.total_rejected(), 1);
        assert_eq!(bridge.aggregator.agent_last_seq(sender_a), None);
        assert_eq!(bridge.aggregator.agent_last_seq(sender_c), Some(1));
        assert_eq!(bridge.diagnostics.duplicates, 1);
        assert_eq!(bridge.diagnostics.pane_reorder_drops, 1);
        assert_eq!(bridge.diagnostics.pane_seq_gap_repairs, 0);
        assert_eq!(
            bridge.diagnostics.replay_event_codes,
            vec!["dist.replay_detected".to_string()]
        );

        assert!(
            panes.iter().any(|pane| {
                pane.pane_id == 71 && pane.domain == "distributed:agent-loss-a:prod"
            }),
            "accepted remote pane should remain visible in state"
        );
        assert!(
            !panes.iter().any(|pane| pane.pane_id == 72),
            "capacity-rejected sender metadata must not persist"
        );
        assert!(
            panes.iter().any(|pane| {
                pane.pane_id == 73 && pane.domain == "distributed:agent-loss-c:prod"
            }),
            "replacement remote pane should be visible after stale prune"
        );
        let segment_contents = segments
            .iter()
            .map(|segment| segment.content.as_str())
            .collect::<Vec<_>>();
        let combined_segments = segment_contents.join("");
        assert!(
            segment_contents
                .iter()
                .any(|content| content.contains("LOSSY_STALE_MARKER")),
            "canonical marker should persist: {segment_contents:?}"
        );
        assert!(
            segment_contents
                .iter()
                .any(|content| content.contains("first")),
            "first canonical payload should persist: {segment_contents:?}"
        );
        assert!(
            segment_contents
                .iter()
                .any(|content| content.contains("after")),
            "after-gap canonical payload should persist: {segment_contents:?}"
        );
        assert!(
            !combined_segments.contains("duplicate") && !combined_segments.contains("replay"),
            "deduped and replay-dropped payloads must not be searchable"
        );
        assert_eq!(
            search_hits.len(),
            2,
            "search should expose persisted output"
        );
        assert!(
            gaps.iter()
                .any(|gap| gap.reason.contains("distributed_out_of_order")),
            "replayed pane sequence should be recorded as a diagnostic gap"
        );
        assert!(
            gaps.iter()
                .any(|gap| gap.reason.contains("dropped_stream_reconnect")),
            "dropped stream should be recorded as an explicit gap"
        );

        emit_artifact(
            "lossy_stale_fault_receipts",
            serde_json::json!({
                "receipts": receipts,
                "state_panes": panes.iter().map(|pane| pane.pane_id).collect::<Vec<_>>(),
                "search_hits": search_hits.len(),
                "gaps": gaps.iter().map(|gap| serde_json::json!({
                    "pane_id": gap.pane_id,
                    "seq_before": gap.seq_before,
                    "seq_after": gap.seq_after,
                    "reason": gap.reason,
                })).collect::<Vec<_>>(),
                "remote_live_read_expectation": {
                    "pane_id": 71,
                    "error_code": "robot.remote_text_unavailable",
                    "mcp_error_code": "mcp.remote_text_unavailable"
                }
            }),
        );
    });
}

#[test]
fn distributed_streaming_e2e_initial_seq_gap_is_persisted_before_first_delta() {
    run_async_test(async {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("distributed_streaming_initial_gap.db");
        let mut bridge = DistributedBridge::new(db_path.to_str().expect("db path"))
            .await
            .expect("bridge");

        let sender = "agent-initial-gap";
        bridge
            .ingest_envelope(WireEnvelope::new(
                1,
                sender,
                WirePayload::PaneMeta(pane_meta(13)),
            ))
            .await
            .expect("meta");
        bridge
            .ingest_envelope(WireEnvelope::new(
                2,
                sender,
                WirePayload::PaneDelta(pane_delta(13, 4, "INITIAL_GAP_MARKER payload")),
            ))
            .await
            .expect("delta after initial gap");

        let gaps = bridge.storage.get_gaps().await.expect("gaps");
        let gap = gaps
            .iter()
            .find(|gap| gap.pane_id == 13)
            .expect("initial seq gap should be persisted");
        assert_eq!(gap.seq_before, 0);
        assert_eq!(gap.seq_after, 4);
        assert!(
            gap.reason
                .contains("distributed_seq_gap:expected=0:actual=4"),
            "gap reason should preserve the inferred initial seq-gap diagnostic"
        );

        let hits = bridge
            .storage
            .search("INITIAL_GAP_MARKER")
            .await
            .expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(bridge.diagnostics.pane_seq_gap_repairs, 1);
    });
}

#[test]
fn distributed_streaming_e2e_rejects_malformed_wire_without_persisting() {
    run_async_test(async {
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
    });
}

#[test]
fn distributed_streaming_e2e_rejects_invalid_sender_and_recovers_with_valid_sender() {
    run_async_test(async {
        use frankenterm_core::wire_protocol::WireProtocolError;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir
            .path()
            .join("distributed_streaming_invalid_sender.db");
        let mut bridge = DistributedBridge::new(db_path.to_str().expect("db path"))
            .await
            .expect("bridge");

        let invalid_err = bridge
            .ingest_envelope(WireEnvelope::new(
                1,
                "agent:invalid",
                WirePayload::PaneMeta(pane_meta(31)),
            ))
            .await
            .expect_err("invalid sender identity should fail");
        let wire_err = invalid_err
            .downcast_ref::<WireProtocolError>()
            .expect("expected WireProtocolError");
        assert!(matches!(wire_err, WireProtocolError::InvalidSender { .. }));
        assert_eq!(bridge.aggregator.total_rejected(), 1);
        assert_eq!(bridge.aggregator.total_accepted(), 0);

        bridge
            .ingest_envelope(WireEnvelope::new(
                2,
                "agent-valid",
                WirePayload::PaneMeta(pane_meta(31)),
            ))
            .await
            .expect("valid sender meta");
        bridge
            .ingest_envelope(WireEnvelope::new(
                3,
                "agent-valid",
                WirePayload::PaneDelta(pane_delta(31, 0, "INVALID_SENDER_RECOVERY_MARKER")),
            ))
            .await
            .expect("valid sender delta");

        let panes = bridge.storage.get_panes().await.expect("panes");
        let hits = bridge
            .storage
            .search("INVALID_SENDER_RECOVERY_MARKER")
            .await
            .expect("search");

        assert_eq!(
            panes.len(),
            1,
            "invalid sender input must not persist pane metadata"
        );
        assert!(panes.iter().any(|pane| pane.pane_id == 31));
        assert_eq!(
            hits.len(),
            1,
            "valid sender should still persist after rejection"
        );
        assert_eq!(bridge.aggregator.total_rejected(), 1);
        assert_eq!(bridge.aggregator.total_accepted(), 2);
    });
}

#[test]
fn distributed_streaming_e2e_rejects_invalid_gap_bounds_and_recovers_with_valid_gap() {
    run_async_test(async {
        use frankenterm_core::wire_protocol::{GapNotice, WireProtocolError};

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir
            .path()
            .join("distributed_streaming_invalid_gap_bounds.db");
        let mut bridge = DistributedBridge::new(db_path.to_str().expect("db path"))
            .await
            .expect("bridge");

        let invalid_err = bridge
            .ingest_envelope(WireEnvelope::new(
                1,
                "agent-gap-invalid",
                WirePayload::Gap(GapNotice {
                    pane_id: 33,
                    seq_before: 4,
                    seq_after: 4,
                    reason: "bad-gap".to_string(),
                    detected_at_ms: now_ms(),
                }),
            ))
            .await
            .expect_err("invalid gap bounds should fail");
        let wire_err = invalid_err
            .downcast_ref::<WireProtocolError>()
            .expect("expected WireProtocolError");
        assert!(matches!(wire_err, WireProtocolError::InvalidJson(_)));
        assert_eq!(bridge.aggregator.total_rejected(), 1);
        assert_eq!(bridge.aggregator.total_accepted(), 0);
        assert!(
            bridge.storage.get_panes().await.expect("panes").is_empty(),
            "invalid gap input must not create pane metadata"
        );
        assert!(
            bridge.storage.get_gaps().await.expect("gaps").is_empty(),
            "invalid gap input must not persist gap records"
        );

        bridge
            .ingest_envelope(WireEnvelope::new(
                2,
                "agent-gap-invalid",
                WirePayload::PaneMeta(pane_meta(33)),
            ))
            .await
            .expect("valid sender meta");
        bridge
            .ingest_envelope(WireEnvelope::new(
                3,
                "agent-gap-invalid",
                WirePayload::Gap(GapNotice {
                    pane_id: 33,
                    seq_before: 0,
                    seq_after: 2,
                    reason: "upstream_reconnect".to_string(),
                    detected_at_ms: now_ms(),
                }),
            ))
            .await
            .expect("valid gap");
        bridge
            .ingest_envelope(WireEnvelope::new(
                4,
                "agent-gap-invalid",
                WirePayload::PaneDelta(pane_delta(33, 2, "VALID_GAP_RECOVERY_MARKER")),
            ))
            .await
            .expect("delta after valid gap");

        let panes = bridge.storage.get_panes().await.expect("panes");
        let gaps = bridge.storage.get_gaps().await.expect("gaps");
        let hits = bridge
            .storage
            .search("VALID_GAP_RECOVERY_MARKER")
            .await
            .expect("search");

        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, 33);
        assert_eq!(hits.len(), 1);
        assert_eq!(gaps.len(), 1, "only the valid explicit gap should persist");
        assert_eq!(gaps[0].pane_id, 33);
        assert_eq!(gaps[0].seq_before, 0);
        assert_eq!(gaps[0].seq_after, 2);
        assert!(
            gaps[0]
                .reason
                .contains("distributed_gap:upstream_reconnect:0:2"),
            "recovery path should preserve the valid explicit gap reason and bounds"
        );
        assert_eq!(bridge.aggregator.total_rejected(), 1);
        assert_eq!(bridge.aggregator.total_accepted(), 3);
        assert_eq!(bridge.diagnostics.pane_seq_gap_repairs, 0);
    });
}

#[test]
fn distributed_streaming_e2e_enforces_agent_capacity_without_cross_sender_persist() {
    run_async_test(async {
        use frankenterm_core::wire_protocol::WireProtocolError;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("distributed_streaming_capacity.db");
        let mut bridge =
            DistributedBridge::new_with_capacity(db_path.to_str().expect("db path"), 1)
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
    });
}

#[test]
fn distributed_streaming_e2e_prunes_stale_sender_and_accepts_new_sender_at_capacity() {
    run_async_test(async {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir
            .path()
            .join("distributed_streaming_stale_capacity.db");
        let mut bridge = DistributedBridge::new_with_capacity_and_stale(
            db_path.to_str().expect("db path"),
            1,
            50,
        )
        .await
        .expect("bridge");

        let mut first = WireEnvelope::new(1, "agent-cap-a", WirePayload::PaneMeta(pane_meta(41)));
        first.sent_at_ms = 50_000;
        bridge
            .ingest_envelope_at(first, 100)
            .await
            .expect("first sender should be accepted");

        let mut second = WireEnvelope::new(1, "agent-cap-b", WirePayload::PaneMeta(pane_meta(42)));
        second.sent_at_ms = 50_001;
        bridge
            .ingest_envelope_at(second, 200)
            .await
            .expect("stale sender should be pruned using local receipt time");

        assert_eq!(bridge.aggregator.total_accepted(), 2);
        assert_eq!(bridge.aggregator.total_rejected(), 0);
        assert_eq!(bridge.aggregator.agent_count(), 1);
        assert_eq!(bridge.aggregator.agent_last_seq("agent-cap-a"), None);
        assert_eq!(bridge.aggregator.agent_last_seq("agent-cap-b"), Some(1));

        let mut panes = bridge.storage.get_panes().await.expect("panes");
        panes.sort_by_key(|pane| pane.pane_id);
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, 41);
        assert_eq!(panes[1].pane_id, 42);
    });
}

#[test]
fn distributed_streaming_e2e_duplicate_refresh_prevents_false_stale_eviction() {
    run_async_test(async {
        use frankenterm_core::wire_protocol::WireProtocolError;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir
            .path()
            .join("distributed_streaming_duplicate_refresh.db");
        let mut bridge = DistributedBridge::new_with_capacity_and_stale(
            db_path.to_str().expect("db path"),
            1,
            50,
        )
        .await
        .expect("bridge");

        let mut first = WireEnvelope::new(1, "agent-dup-a", WirePayload::PaneMeta(pane_meta(51)));
        first.sent_at_ms = 100;
        bridge
            .ingest_envelope_at(first, 100)
            .await
            .expect("first sender should be accepted");

        let mut duplicate =
            WireEnvelope::new(1, "agent-dup-a", WirePayload::PaneMeta(pane_meta(51)));
        duplicate.sent_at_ms = 1;
        bridge
            .ingest_envelope_at(duplicate, 130)
            .await
            .expect("duplicate should refresh liveness without persisting");

        let mut second = WireEnvelope::new(1, "agent-dup-b", WirePayload::PaneMeta(pane_meta(52)));
        second.sent_at_ms = 10_000;
        let err = bridge
            .ingest_envelope_at(second, 160)
            .await
            .expect_err("recent duplicate should keep first sender from being pruned");
        let wire_err = err
            .downcast_ref::<WireProtocolError>()
            .expect("expected WireProtocolError");
        assert!(matches!(
            wire_err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(bridge.aggregator.total_accepted(), 1);
        assert_eq!(bridge.aggregator.total_rejected(), 1);

        let panes = bridge.storage.get_panes().await.expect("panes");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, 51);
    });
}

#[test]
fn distributed_streaming_e2e_accepted_regressed_timestamp_does_not_trigger_false_stale_prune() {
    run_async_test(async {
        use frankenterm_core::wire_protocol::WireProtocolError;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir
            .path()
            .join("distributed_streaming_regressed_accepted_liveness.db");
        let mut bridge = DistributedBridge::new_with_capacity_and_stale(
            db_path.to_str().expect("db path"),
            1,
            50,
        )
        .await
        .expect("bridge");

        let mut first = WireEnvelope::new(1, "agent-reg-a", WirePayload::PaneMeta(pane_meta(61)));
        first.sent_at_ms = 100;
        bridge
            .ingest_envelope_at(first, 100)
            .await
            .expect("first sender should be accepted");

        // Sender clock regresses, but this is still a new accepted sequence.
        let mut regressed =
            WireEnvelope::new(2, "agent-reg-a", WirePayload::PaneMeta(pane_meta(61)));
        regressed.sent_at_ms = 90;
        bridge
            .ingest_envelope_at(regressed, 140)
            .await
            .expect("regressed timestamp on accepted seq should not evict sender liveness");

        let mut second = WireEnvelope::new(1, "agent-reg-b", WirePayload::PaneMeta(pane_meta(62)));
        second.sent_at_ms = 1_000;
        let err = bridge.ingest_envelope_at(second, 180).await.expect_err(
            "sender-a should remain recent based on receipt time, so capacity reject should fire",
        );
        let wire_err = err
            .downcast_ref::<WireProtocolError>()
            .expect("expected WireProtocolError");
        assert!(matches!(
            wire_err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(bridge.aggregator.total_accepted(), 2);
        assert_eq!(bridge.aggregator.total_rejected(), 1);
        assert_eq!(bridge.aggregator.agent_last_seq("agent-reg-a"), Some(2));
        assert_eq!(bridge.aggregator.agent_last_seq("agent-reg-b"), None);

        let panes = bridge.storage.get_panes().await.expect("panes");
        assert_eq!(
            panes.len(),
            1,
            "rejected sender metadata must not persist after false-stale guard"
        );
        assert_eq!(panes[0].pane_id, 61);
    });
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
