//! Wire protocol message types for distributed wa communication.
//!
//! Defines versioned message envelopes exchanged between `wa-agent` instances
//! and an aggregator. All messages are JSON-serializable, timestamped in epoch
//! milliseconds, and include a protocol version for forward/backward compat.

use serde::{Deserialize, Serialize};

use crate::patterns::{AgentType, Severity};

/// Current protocol version. Bump on breaking changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum allowed message payload size in bytes (1 MiB).
pub const MAX_MESSAGE_SIZE: usize = 1_048_576;

// ---------------------------------------------------------------------------
// Core wire messages
// ---------------------------------------------------------------------------

/// Pane metadata broadcast when a pane is first discovered or updated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneMeta {
    pub pane_id: u64,
    pub pane_uuid: Option<String>,
    pub domain: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
    pub observed: bool,
    pub timestamp_ms: i64,
}

/// A captured output delta from a pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneDelta {
    pub pane_id: u64,
    pub seq: u64,
    pub content: String,
    pub content_len: usize,
    pub captured_at_ms: i64,
}

/// A gap in the capture stream (e.g., daemon restart, timeout).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapNotice {
    pub pane_id: u64,
    pub seq_before: u64,
    pub seq_after: u64,
    pub reason: String,
    pub detected_at_ms: i64,
}

/// A detection event from the pattern engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectionNotice {
    pub rule_id: String,
    pub agent_type: AgentType,
    pub event_type: String,
    pub severity: Severity,
    pub confidence: f64,
    pub extracted: serde_json::Value,
    pub matched_text: String,
    pub pane_id: u64,
    pub pane_uuid: Option<String>,
    pub detected_at_ms: i64,
}

/// Snapshot of all currently known panes (periodic heartbeat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanesMeta {
    pub panes: Vec<PaneMeta>,
    pub timestamp_ms: i64,
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// All possible wire message payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WirePayload {
    PaneMeta(PaneMeta),
    PaneDelta(PaneDelta),
    Gap(GapNotice),
    Detection(DetectionNotice),
    PanesMeta(PanesMeta),
}

/// Versioned envelope wrapping every wire message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireEnvelope {
    /// Protocol version for compat checking.
    pub version: u32,
    /// Monotonically increasing per sender for ordering and dedup.
    pub seq: u64,
    /// Sender identity (hostname or agent id).
    pub sender: String,
    /// Epoch-ms timestamp when the message was created.
    pub sent_at_ms: i64,
    /// The actual payload.
    pub payload: WirePayload,
}

impl WireEnvelope {
    /// Create a new envelope with the current protocol version.
    pub fn new(seq: u64, sender: impl Into<String>, payload: WirePayload) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            seq,
            sender: sender.into(),
            sent_at_ms: epoch_ms_now(),
            payload,
        }
    }

    /// Serialize to JSON bytes.
    pub fn to_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Deserialize from JSON bytes with size validation.
    pub fn from_json(bytes: &[u8]) -> Result<Self, WireProtocolError> {
        if bytes.len() > MAX_MESSAGE_SIZE {
            return Err(WireProtocolError::MessageTooLarge {
                size: bytes.len(),
                max: MAX_MESSAGE_SIZE,
            });
        }
        let envelope: Self =
            serde_json::from_slice(bytes).map_err(WireProtocolError::InvalidJson)?;
        if envelope.version != PROTOCOL_VERSION {
            return Err(WireProtocolError::VersionMismatch {
                expected: PROTOCOL_VERSION,
                got: envelope.version,
            });
        }
        Ok(envelope)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from wire protocol encode/decode.
#[derive(Debug, thiserror::Error)]
pub enum WireProtocolError {
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("message too large: {size} bytes (max {max})")]
    MessageTooLarge { size: usize, max: usize },
    #[error("protocol version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u32, got: u32 },
}

// ---------------------------------------------------------------------------
// Agent streamer: converts EventBus events to WireEnvelopes
// ---------------------------------------------------------------------------

use crate::events::Event;

/// Connection state for the agent streamer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

/// Configuration for exponential backoff on reconnect.
#[derive(Debug, Clone)]
pub struct BackoffConfig {
    pub initial_ms: u64,
    pub max_ms: u64,
    pub multiplier: f64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial_ms: 500,
            max_ms: 30_000,
            multiplier: 2.0,
        }
    }
}

impl BackoffConfig {
    /// Calculate delay for a given attempt number (0-based).
    #[must_use]
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        let delay = self.initial_ms as f64 * self.multiplier.powi(attempt as i32);
        (delay as u64).min(self.max_ms)
    }
}

/// Converts `Event` bus events into `WireEnvelope` messages for streaming.
///
/// The streamer is transport-agnostic: it produces serialized messages
/// that a transport layer (WebSocket, TCP, etc.) sends to the aggregator.
pub struct AgentStreamer {
    sender_id: String,
    seq: u64,
    state: ConnectionState,
    backoff: BackoffConfig,
    messages_sent: u64,
    messages_dropped: u64,
}

impl AgentStreamer {
    /// Create a new agent streamer with the given sender identity.
    pub fn new(sender_id: impl Into<String>) -> Self {
        Self {
            sender_id: sender_id.into(),
            seq: 0,
            state: ConnectionState::Disconnected,
            backoff: BackoffConfig::default(),
            messages_sent: 0,
            messages_dropped: 0,
        }
    }

    /// Create with custom backoff config.
    pub fn with_backoff(sender_id: impl Into<String>, backoff: BackoffConfig) -> Self {
        Self {
            sender_id: sender_id.into(),
            seq: 0,
            state: ConnectionState::Disconnected,
            backoff,
            messages_sent: 0,
            messages_dropped: 0,
        }
    }

    /// Current connection state.
    #[must_use]
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Total messages successfully produced.
    #[must_use]
    pub fn messages_sent(&self) -> u64 {
        self.messages_sent
    }

    /// Messages dropped due to conversion failure.
    #[must_use]
    pub fn messages_dropped(&self) -> u64 {
        self.messages_dropped
    }

    /// Current sequence number.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Transition to connected state.
    pub fn mark_connected(&mut self) {
        self.state = ConnectionState::Connected;
    }

    /// Transition to reconnecting state, returning the backoff delay in ms.
    pub fn mark_reconnecting(&mut self) -> u64 {
        let attempt = match self.state {
            ConnectionState::Reconnecting { attempt } => attempt + 1,
            _ => 0,
        };
        self.state = ConnectionState::Reconnecting { attempt };
        self.backoff.delay_ms(attempt)
    }

    /// Transition to disconnected state.
    pub fn mark_disconnected(&mut self) {
        self.state = ConnectionState::Disconnected;
    }

    /// Convert an EventBus event to a WireEnvelope, if the event maps to a
    /// wire message. Returns `None` for events that don't need streaming
    /// (workflow internal state, user-var internals).
    pub fn event_to_envelope(&mut self, event: &Event) -> Option<WireEnvelope> {
        let payload = match event {
            Event::SegmentCaptured {
                pane_id,
                seq,
                content_len,
            } => Some(WirePayload::PaneDelta(PaneDelta {
                pane_id: *pane_id,
                seq: *seq,
                content: String::new(), // Content not in bus event; caller fills from storage
                content_len: *content_len,
                captured_at_ms: epoch_ms_now(),
            })),

            Event::GapDetected { pane_id, reason } => Some(WirePayload::Gap(GapNotice {
                pane_id: *pane_id,
                seq_before: 0, // Caller fills from storage
                seq_after: 0,
                reason: reason.clone(),
                detected_at_ms: epoch_ms_now(),
            })),

            Event::PatternDetected {
                pane_id,
                pane_uuid,
                detection,
                ..
            } => Some(WirePayload::Detection(DetectionNotice {
                rule_id: detection.rule_id.clone(),
                agent_type: detection.agent_type,
                event_type: detection.event_type.clone(),
                severity: detection.severity,
                confidence: detection.confidence,
                extracted: detection.extracted.clone(),
                matched_text: detection.matched_text.clone(),
                pane_id: *pane_id,
                pane_uuid: pane_uuid.clone(),
                detected_at_ms: epoch_ms_now(),
            })),

            Event::PaneDiscovered {
                pane_id,
                domain,
                title,
            } => Some(WirePayload::PaneMeta(PaneMeta {
                pane_id: *pane_id,
                pane_uuid: None,
                domain: domain.clone(),
                title: Some(title.clone()),
                cwd: None,
                rows: None,
                cols: None,
                observed: true,
                timestamp_ms: epoch_ms_now(),
            })),

            // Workflow events and user-var events are local-only; not streamed.
            Event::PaneDisappeared { .. }
            | Event::WorkflowStarted { .. }
            | Event::WorkflowStep { .. }
            | Event::WorkflowCompleted { .. }
            | Event::UserVarReceived { .. } => None,
        };

        payload.map(|p| {
            self.seq += 1;
            self.messages_sent += 1;
            WireEnvelope::new(self.seq, &self.sender_id, p)
        })
    }
}

// ---------------------------------------------------------------------------
// Aggregator: accepts and processes incoming agent streams
// ---------------------------------------------------------------------------

use std::collections::HashMap;

/// Per-agent tracking state within the aggregator.
#[derive(Debug)]
struct AgentSession {
    /// Last sequence number received from this agent (for ordering/dedup).
    last_seq: u64,
    /// Total messages received from this agent.
    messages_received: u64,
    /// Total duplicates skipped.
    duplicates_skipped: u64,
    /// Timestamp of last message.
    last_seen_ms: i64,
}

/// Result of processing an incoming wire message.
#[derive(Debug, Clone, PartialEq)]
pub enum IngestResult {
    /// Message accepted and payload extracted.
    Accepted(WirePayload),
    /// Duplicate message (already seen this seq from this sender).
    Duplicate { sender: String, seq: u64 },
}

/// Aggregator that processes incoming wire messages from agents.
///
/// Provides per-agent dedup, ordering validation, and metrics.
/// Transport-agnostic: the caller feeds raw JSON bytes, the aggregator
/// returns processed payloads ready for the event bus / storage.
pub struct Aggregator {
    agents: HashMap<String, AgentSession>,
    total_accepted: u64,
    total_rejected: u64,
    max_agents: usize,
}

impl Aggregator {
    /// Create a new aggregator with a maximum number of tracked agents.
    pub fn new(max_agents: usize) -> Self {
        Self {
            agents: HashMap::new(),
            total_accepted: 0,
            total_rejected: 0,
            max_agents,
        }
    }

    /// Process a raw JSON wire message. Returns the payload if accepted.
    pub fn ingest(&mut self, bytes: &[u8]) -> Result<IngestResult, WireProtocolError> {
        let envelope = WireEnvelope::from_json(bytes)?;
        self.ingest_envelope(envelope)
    }

    /// Process a decoded envelope. Returns the payload if accepted.
    pub fn ingest_envelope(
        &mut self,
        envelope: WireEnvelope,
    ) -> Result<IngestResult, WireProtocolError> {
        let is_new = !self.agents.contains_key(&envelope.sender);
        if is_new && self.agents.len() >= self.max_agents {
            tracing::warn!(
                "aggregator: max agents ({}) reached, allowing new agent '{}'",
                self.max_agents,
                envelope.sender
            );
        }

        let session = self
            .agents
            .entry(envelope.sender.clone())
            .or_insert(AgentSession {
                last_seq: 0,
                messages_received: 0,
                duplicates_skipped: 0,
                last_seen_ms: 0,
            });

        // Dedup: skip if we've already seen this or a later seq from this sender.
        if envelope.seq <= session.last_seq {
            session.duplicates_skipped += 1;
            return Ok(IngestResult::Duplicate {
                sender: envelope.sender,
                seq: envelope.seq,
            });
        }

        session.last_seq = envelope.seq;
        session.messages_received += 1;
        session.last_seen_ms = envelope.sent_at_ms;
        self.total_accepted += 1;

        Ok(IngestResult::Accepted(envelope.payload))
    }

    /// Number of unique agents currently tracked.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Total accepted messages across all agents.
    #[must_use]
    pub fn total_accepted(&self) -> u64 {
        self.total_accepted
    }

    /// Total rejected messages (parse errors, etc.).
    #[must_use]
    pub fn total_rejected(&self) -> u64 {
        self.total_rejected
    }

    /// Get the last sequence number received from a given agent.
    #[must_use]
    pub fn agent_last_seq(&self, sender: &str) -> Option<u64> {
        self.agents.get(sender).map(|s| s.last_seq)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn epoch_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pane_meta() -> PaneMeta {
        PaneMeta {
            pane_id: 42,
            pane_uuid: Some("abc-def-123".into()),
            domain: "local".into(),
            title: Some("codex".into()),
            cwd: Some("/home/user/project".into()),
            rows: Some(24),
            cols: Some(80),
            observed: true,
            timestamp_ms: 1_700_000_000_000,
        }
    }

    fn sample_pane_delta() -> PaneDelta {
        PaneDelta {
            pane_id: 42,
            seq: 7,
            content: "Token usage: total=1000".into(),
            content_len: 23,
            captured_at_ms: 1_700_000_001_000,
        }
    }

    fn sample_gap() -> GapNotice {
        GapNotice {
            pane_id: 42,
            seq_before: 5,
            seq_after: 10,
            reason: "daemon_restart".into(),
            detected_at_ms: 1_700_000_002_000,
        }
    }

    fn sample_detection() -> DetectionNotice {
        DetectionNotice {
            rule_id: "codex.usage.reached".into(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".into(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({"reset_time": "2:30 PM"}),
            matched_text: "You've hit your usage limit".into(),
            pane_id: 42,
            pane_uuid: Some("abc-def-123".into()),
            detected_at_ms: 1_700_000_003_000,
        }
    }

    fn sample_panes_meta() -> PanesMeta {
        PanesMeta {
            panes: vec![sample_pane_meta()],
            timestamp_ms: 1_700_000_004_000,
        }
    }

    // --- Round-trip tests ---

    #[test]
    fn roundtrip_pane_meta() {
        let envelope = WireEnvelope::new(1, "agent-1", WirePayload::PaneMeta(sample_pane_meta()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(envelope.version, decoded.version);
        assert_eq!(envelope.seq, decoded.seq);
        assert_eq!(envelope.sender, decoded.sender);
        assert_eq!(envelope.payload, decoded.payload);
    }

    #[test]
    fn roundtrip_pane_delta() {
        let envelope = WireEnvelope::new(2, "agent-1", WirePayload::PaneDelta(sample_pane_delta()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(envelope.payload, decoded.payload);
    }

    #[test]
    fn roundtrip_gap() {
        let envelope = WireEnvelope::new(3, "agent-1", WirePayload::Gap(sample_gap()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(envelope.payload, decoded.payload);
    }

    #[test]
    fn roundtrip_detection() {
        let envelope = WireEnvelope::new(4, "agent-1", WirePayload::Detection(sample_detection()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(envelope.payload, decoded.payload);
    }

    #[test]
    fn roundtrip_panes_meta() {
        let envelope = WireEnvelope::new(5, "agent-1", WirePayload::PanesMeta(sample_panes_meta()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(envelope.payload, decoded.payload);
    }

    // --- Envelope fields ---

    #[test]
    fn envelope_has_correct_version() {
        let envelope = WireEnvelope::new(1, "test", WirePayload::PaneMeta(sample_pane_meta()));
        assert_eq!(envelope.version, PROTOCOL_VERSION);
    }

    #[test]
    fn envelope_sender_preserved() {
        let envelope =
            WireEnvelope::new(1, "my-hostname", WirePayload::PaneMeta(sample_pane_meta()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(decoded.sender, "my-hostname");
    }

    #[test]
    fn envelope_seq_preserved() {
        let envelope = WireEnvelope::new(999, "agent", WirePayload::PaneDelta(sample_pane_delta()));
        let bytes = envelope.to_json().unwrap();
        let decoded = WireEnvelope::from_json(&bytes).unwrap();
        assert_eq!(decoded.seq, 999);
    }

    // --- Tagged JSON format ---

    #[test]
    fn json_has_type_tag() {
        let envelope = WireEnvelope::new(1, "a", WirePayload::PaneMeta(sample_pane_meta()));
        let json_str = serde_json::to_string(&envelope).unwrap();
        assert!(json_str.contains("\"type\":\"pane_meta\""));

        let envelope = WireEnvelope::new(1, "a", WirePayload::PaneDelta(sample_pane_delta()));
        let json_str = serde_json::to_string(&envelope).unwrap();
        assert!(json_str.contains("\"type\":\"pane_delta\""));

        let envelope = WireEnvelope::new(1, "a", WirePayload::Gap(sample_gap()));
        let json_str = serde_json::to_string(&envelope).unwrap();
        assert!(json_str.contains("\"type\":\"gap\""));

        let envelope = WireEnvelope::new(1, "a", WirePayload::Detection(sample_detection()));
        let json_str = serde_json::to_string(&envelope).unwrap();
        assert!(json_str.contains("\"type\":\"detection\""));

        let envelope = WireEnvelope::new(1, "a", WirePayload::PanesMeta(sample_panes_meta()));
        let json_str = serde_json::to_string(&envelope).unwrap();
        assert!(json_str.contains("\"type\":\"panes_meta\""));
    }

    // --- Error handling ---

    #[test]
    fn rejects_oversized_message() {
        let huge = vec![b'{'; MAX_MESSAGE_SIZE + 1];
        let err = WireEnvelope::from_json(&huge).unwrap_err();
        assert!(
            matches!(err, WireProtocolError::MessageTooLarge { .. }),
            "expected MessageTooLarge, got: {err}"
        );
    }

    #[test]
    fn rejects_malformed_json() {
        let err = WireEnvelope::from_json(b"not json at all").unwrap_err();
        assert!(matches!(err, WireProtocolError::InvalidJson(_)));
    }

    #[test]
    fn rejects_version_mismatch() {
        let mut envelope = WireEnvelope::new(1, "a", WirePayload::Gap(sample_gap()));
        envelope.version = 999;
        let bytes = envelope.to_json().unwrap();
        let err = WireEnvelope::from_json(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                WireProtocolError::VersionMismatch {
                    expected: 1,
                    got: 999
                }
            ),
            "expected VersionMismatch, got: {err}"
        );
    }

    #[test]
    fn empty_bytes_rejected() {
        let err = WireEnvelope::from_json(b"").unwrap_err();
        assert!(matches!(err, WireProtocolError::InvalidJson(_)));
    }

    // --- Golden fixture: a known-good serialized message ---

    #[test]
    fn golden_detection_fixture() {
        let json = r#"{
            "version": 1,
            "seq": 42,
            "sender": "agent-alpha",
            "sent_at_ms": 1700000003000,
            "payload": {
                "type": "detection",
                "rule_id": "codex.usage.reached",
                "agent_type": "codex",
                "event_type": "usage.reached",
                "severity": "critical",
                "confidence": 1.0,
                "extracted": {"reset_time": "2:30 PM"},
                "matched_text": "You've hit your usage limit",
                "pane_id": 42,
                "pane_uuid": "abc-def-123",
                "detected_at_ms": 1700000003000
            }
        }"#;
        let envelope = WireEnvelope::from_json(json.as_bytes()).unwrap();
        assert_eq!(envelope.seq, 42);
        assert_eq!(envelope.sender, "agent-alpha");
        match &envelope.payload {
            WirePayload::Detection(d) => {
                assert_eq!(d.rule_id, "codex.usage.reached");
                assert_eq!(d.agent_type, AgentType::Codex);
                assert_eq!(d.severity, Severity::Critical);
                assert_eq!(d.extracted["reset_time"], "2:30 PM");
            }
            other => panic!("expected Detection, got: {other:?}"),
        }
    }

    #[test]
    fn golden_pane_delta_fixture() {
        let json = r#"{
            "version": 1,
            "seq": 100,
            "sender": "agent-beta",
            "sent_at_ms": 1700000001000,
            "payload": {
                "type": "pane_delta",
                "pane_id": 7,
                "seq": 55,
                "content": "Hello, world!",
                "content_len": 13,
                "captured_at_ms": 1700000001000
            }
        }"#;
        let envelope = WireEnvelope::from_json(json.as_bytes()).unwrap();
        match &envelope.payload {
            WirePayload::PaneDelta(d) => {
                assert_eq!(d.pane_id, 7);
                assert_eq!(d.seq, 55);
                assert_eq!(d.content, "Hello, world!");
            }
            other => panic!("expected PaneDelta, got: {other:?}"),
        }
    }

    #[test]
    fn golden_gap_fixture() {
        let json = r#"{
            "version": 1,
            "seq": 3,
            "sender": "agent-gamma",
            "sent_at_ms": 1700000002000,
            "payload": {
                "type": "gap",
                "pane_id": 42,
                "seq_before": 5,
                "seq_after": 10,
                "reason": "daemon_restart",
                "detected_at_ms": 1700000002000
            }
        }"#;
        let envelope = WireEnvelope::from_json(json.as_bytes()).unwrap();
        match &envelope.payload {
            WirePayload::Gap(g) => {
                assert_eq!(g.pane_id, 42);
                assert_eq!(g.seq_before, 5);
                assert_eq!(g.seq_after, 10);
                assert_eq!(g.reason, "daemon_restart");
            }
            other => panic!("expected Gap, got: {other:?}"),
        }
    }

    #[test]
    fn golden_panes_meta_fixture() {
        let json = r#"{
            "version": 1,
            "seq": 1,
            "sender": "aggregator",
            "sent_at_ms": 1700000004000,
            "payload": {
                "type": "panes_meta",
                "panes": [
                    {
                        "pane_id": 0,
                        "pane_uuid": null,
                        "domain": "local",
                        "title": "bash",
                        "cwd": "/home/user",
                        "rows": 24,
                        "cols": 80,
                        "observed": true,
                        "timestamp_ms": 1700000000000
                    }
                ],
                "timestamp_ms": 1700000004000
            }
        }"#;
        let envelope = WireEnvelope::from_json(json.as_bytes()).unwrap();
        match &envelope.payload {
            WirePayload::PanesMeta(pm) => {
                assert_eq!(pm.panes.len(), 1);
                assert_eq!(pm.panes[0].domain, "local");
            }
            other => panic!("expected PanesMeta, got: {other:?}"),
        }
    }

    // --- Forward compatibility: extra fields are ignored ---

    #[test]
    fn extra_fields_in_payload_ignored() {
        let json = r#"{
            "version": 1,
            "seq": 1,
            "sender": "test",
            "sent_at_ms": 0,
            "future_field": "should be ignored",
            "payload": {
                "type": "gap",
                "pane_id": 1,
                "seq_before": 0,
                "seq_after": 2,
                "reason": "test",
                "detected_at_ms": 0,
                "new_field": "also ignored"
            }
        }"#;
        let envelope = WireEnvelope::from_json(json.as_bytes()).unwrap();
        assert!(matches!(envelope.payload, WirePayload::Gap(_)));
    }

    // --- Agent streamer tests ---

    #[test]
    fn streamer_converts_segment_captured() {
        let mut streamer = AgentStreamer::new("test-agent");
        let event = Event::SegmentCaptured {
            pane_id: 1,
            seq: 42,
            content_len: 100,
        };
        let envelope = streamer.event_to_envelope(&event).unwrap();
        assert_eq!(envelope.seq, 1);
        assert_eq!(envelope.sender, "test-agent");
        match &envelope.payload {
            WirePayload::PaneDelta(d) => {
                assert_eq!(d.pane_id, 1);
                assert_eq!(d.seq, 42);
                assert_eq!(d.content_len, 100);
            }
            other => panic!("expected PaneDelta, got: {other:?}"),
        }
    }

    #[test]
    fn streamer_converts_gap_detected() {
        let mut streamer = AgentStreamer::new("test");
        let event = Event::GapDetected {
            pane_id: 5,
            reason: "timeout".into(),
        };
        let envelope = streamer.event_to_envelope(&event).unwrap();
        match &envelope.payload {
            WirePayload::Gap(g) => {
                assert_eq!(g.pane_id, 5);
                assert_eq!(g.reason, "timeout");
            }
            other => panic!("expected Gap, got: {other:?}"),
        }
    }

    #[test]
    fn streamer_converts_pattern_detected() {
        use crate::patterns::Detection;

        let mut streamer = AgentStreamer::new("test");
        let event = Event::PatternDetected {
            pane_id: 3,
            pane_uuid: Some("uuid-123".into()),
            detection: Detection {
                rule_id: "codex.usage.reached".into(),
                agent_type: AgentType::Codex,
                event_type: "usage.reached".into(),
                severity: Severity::Critical,
                confidence: 1.0,
                extracted: serde_json::json!({"reset_time": "3 PM"}),
                matched_text: "limit reached".into(),
                span: (0, 13),
            },
            event_id: Some(99),
        };
        let envelope = streamer.event_to_envelope(&event).unwrap();
        match &envelope.payload {
            WirePayload::Detection(d) => {
                assert_eq!(d.rule_id, "codex.usage.reached");
                assert_eq!(d.pane_uuid, Some("uuid-123".into()));
            }
            other => panic!("expected Detection, got: {other:?}"),
        }
    }

    #[test]
    fn streamer_converts_pane_discovered() {
        let mut streamer = AgentStreamer::new("test");
        let event = Event::PaneDiscovered {
            pane_id: 10,
            domain: "SSH:prod".into(),
            title: "claude-code".into(),
        };
        let envelope = streamer.event_to_envelope(&event).unwrap();
        match &envelope.payload {
            WirePayload::PaneMeta(pm) => {
                assert_eq!(pm.pane_id, 10);
                assert_eq!(pm.domain, "SSH:prod");
                assert_eq!(pm.title, Some("claude-code".into()));
            }
            other => panic!("expected PaneMeta, got: {other:?}"),
        }
    }

    #[test]
    fn streamer_skips_workflow_events() {
        let mut streamer = AgentStreamer::new("test");
        let events = vec![
            Event::WorkflowStarted {
                workflow_id: "w1".into(),
                workflow_name: "test".into(),
                pane_id: 1,
            },
            Event::WorkflowStep {
                workflow_id: "w1".into(),
                step_name: "step1".into(),
                result: "ok".into(),
            },
            Event::WorkflowCompleted {
                workflow_id: "w1".into(),
                success: true,
                reason: None,
            },
            Event::PaneDisappeared { pane_id: 1 },
        ];
        for event in &events {
            assert!(
                streamer.event_to_envelope(event).is_none(),
                "workflow/pane-disappeared events should not produce wire messages"
            );
        }
        assert_eq!(streamer.messages_sent(), 0);
    }

    #[test]
    fn streamer_seq_increments() {
        let mut streamer = AgentStreamer::new("test");
        let events = vec![
            Event::SegmentCaptured {
                pane_id: 1,
                seq: 1,
                content_len: 10,
            },
            Event::GapDetected {
                pane_id: 1,
                reason: "test".into(),
            },
            Event::PaneDiscovered {
                pane_id: 2,
                domain: "local".into(),
                title: "bash".into(),
            },
        ];
        for (i, event) in events.iter().enumerate() {
            let env = streamer.event_to_envelope(event).unwrap();
            assert_eq!(env.seq, (i + 1) as u64);
        }
        assert_eq!(streamer.seq(), 3);
        assert_eq!(streamer.messages_sent(), 3);
    }

    // --- Connection state machine ---

    #[test]
    fn streamer_initial_state_disconnected() {
        let streamer = AgentStreamer::new("test");
        assert_eq!(streamer.state(), ConnectionState::Disconnected);
    }

    #[test]
    fn streamer_state_transitions() {
        let mut streamer = AgentStreamer::new("test");

        streamer.mark_connected();
        assert_eq!(streamer.state(), ConnectionState::Connected);

        let delay = streamer.mark_reconnecting();
        assert_eq!(
            streamer.state(),
            ConnectionState::Reconnecting { attempt: 0 }
        );
        assert_eq!(delay, 500); // initial_ms

        let delay = streamer.mark_reconnecting();
        assert_eq!(
            streamer.state(),
            ConnectionState::Reconnecting { attempt: 1 }
        );
        assert_eq!(delay, 1000); // 500 * 2.0

        streamer.mark_disconnected();
        assert_eq!(streamer.state(), ConnectionState::Disconnected);
    }

    // --- Backoff tests ---

    #[test]
    fn backoff_exponential() {
        let cfg = BackoffConfig::default();
        assert_eq!(cfg.delay_ms(0), 500);
        assert_eq!(cfg.delay_ms(1), 1000);
        assert_eq!(cfg.delay_ms(2), 2000);
        assert_eq!(cfg.delay_ms(3), 4000);
        assert_eq!(cfg.delay_ms(4), 8000);
        assert_eq!(cfg.delay_ms(5), 16000);
    }

    #[test]
    fn backoff_capped_at_max() {
        let cfg = BackoffConfig {
            initial_ms: 1000,
            max_ms: 5000,
            multiplier: 3.0,
        };
        assert_eq!(cfg.delay_ms(0), 1000);
        assert_eq!(cfg.delay_ms(1), 3000);
        assert_eq!(cfg.delay_ms(2), 5000); // capped
        assert_eq!(cfg.delay_ms(10), 5000); // still capped
    }

    #[test]
    fn backoff_reconnect_resets_on_connect() {
        let mut streamer = AgentStreamer::new("test");

        // Reconnect several times
        streamer.mark_reconnecting(); // attempt 0
        streamer.mark_reconnecting(); // attempt 1
        streamer.mark_reconnecting(); // attempt 2

        // Then connect
        streamer.mark_connected();
        assert_eq!(streamer.state(), ConnectionState::Connected);

        // Next reconnect starts from attempt 0
        let delay = streamer.mark_reconnecting();
        assert_eq!(delay, 500); // back to initial
    }

    // --- Aggregator tests ---

    #[test]
    fn aggregator_accepts_valid_message() {
        let mut agg = Aggregator::new(10);
        let envelope = WireEnvelope::new(1, "agent-1", WirePayload::Gap(sample_gap()));
        let bytes = envelope.to_json().unwrap();
        let result = agg.ingest(&bytes).unwrap();
        assert!(matches!(result, IngestResult::Accepted(WirePayload::Gap(_))));
        assert_eq!(agg.total_accepted(), 1);
        assert_eq!(agg.agent_count(), 1);
    }

    #[test]
    fn aggregator_dedup_by_seq() {
        let mut agg = Aggregator::new(10);
        let e1 = WireEnvelope::new(1, "agent-1", WirePayload::Gap(sample_gap()));
        let e2 = WireEnvelope::new(1, "agent-1", WirePayload::Gap(sample_gap())); // same seq
        let e3 = WireEnvelope::new(2, "agent-1", WirePayload::Gap(sample_gap())); // new seq

        assert!(matches!(
            agg.ingest_envelope(e1).unwrap(),
            IngestResult::Accepted(_)
        ));
        assert!(matches!(
            agg.ingest_envelope(e2).unwrap(),
            IngestResult::Duplicate { .. }
        ));
        assert!(matches!(
            agg.ingest_envelope(e3).unwrap(),
            IngestResult::Accepted(_)
        ));
        assert_eq!(agg.total_accepted(), 2);
    }

    #[test]
    fn aggregator_tracks_multiple_agents() {
        let mut agg = Aggregator::new(10);
        let e1 = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let e2 = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        let e3 = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));

        agg.ingest_envelope(e1).unwrap();
        agg.ingest_envelope(e2).unwrap();
        agg.ingest_envelope(e3).unwrap();

        assert_eq!(agg.agent_count(), 2);
        assert_eq!(agg.agent_last_seq("agent-a"), Some(2));
        assert_eq!(agg.agent_last_seq("agent-b"), Some(1));
        assert_eq!(agg.agent_last_seq("unknown"), None);
    }

    #[test]
    fn aggregator_rejects_malformed_input() {
        let mut agg = Aggregator::new(10);
        let result = agg.ingest(b"not json");
        assert!(result.is_err());
    }

    #[test]
    fn aggregator_rejects_oversized_input() {
        let mut agg = Aggregator::new(10);
        let huge = vec![b'{'; MAX_MESSAGE_SIZE + 1];
        let result = agg.ingest(&huge);
        assert!(matches!(
            result,
            Err(WireProtocolError::MessageTooLarge { .. })
        ));
    }

    #[test]
    fn aggregator_end_to_end_with_streamer() {
        let mut streamer = AgentStreamer::new("remote-agent");
        let mut agg = Aggregator::new(10);

        // Simulate: streamer produces events, aggregator consumes them
        let events = vec![
            Event::PaneDiscovered {
                pane_id: 1,
                domain: "SSH:prod".into(),
                title: "codex".into(),
            },
            Event::SegmentCaptured {
                pane_id: 1,
                seq: 1,
                content_len: 50,
            },
            Event::GapDetected {
                pane_id: 1,
                reason: "restart".into(),
            },
        ];

        for event in &events {
            if let Some(envelope) = streamer.event_to_envelope(event) {
                let bytes = envelope.to_json().unwrap();
                let result = agg.ingest(&bytes).unwrap();
                assert!(matches!(result, IngestResult::Accepted(_)));
            }
        }

        assert_eq!(agg.total_accepted(), 3);
        assert_eq!(agg.agent_last_seq("remote-agent"), Some(3));
    }

    #[test]
    fn aggregator_old_seq_skipped() {
        let mut agg = Aggregator::new(10);
        // Receive seq 5 first
        let e1 = WireEnvelope::new(5, "agent", WirePayload::Gap(sample_gap()));
        agg.ingest_envelope(e1).unwrap();

        // Then receive seq 3 (out-of-order/old) - should be skipped
        let e2 = WireEnvelope::new(3, "agent", WirePayload::Gap(sample_gap()));
        let result = agg.ingest_envelope(e2).unwrap();
        assert!(matches!(result, IngestResult::Duplicate { .. }));

        // seq 6 accepted
        let e3 = WireEnvelope::new(6, "agent", WirePayload::Gap(sample_gap()));
        let result = agg.ingest_envelope(e3).unwrap();
        assert!(matches!(result, IngestResult::Accepted(_)));
    }
}
