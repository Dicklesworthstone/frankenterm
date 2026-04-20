//! Wire protocol message types for distributed wa communication.
//!
//! Defines versioned message envelopes exchanged between `wa-agent` instances
//! and an aggregator. All messages are JSON-serializable, timestamped in epoch
//! milliseconds, and include a protocol version for forward/backward compat.

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::patterns::{AgentType, Severity};

/// Current protocol version. Bump on breaking changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum allowed message payload size in bytes (1 MiB).
/// Overridable via `[tuning.wire_protocol] max_message_size` in ft.toml.
/// Both sender and receiver must agree on this value in distributed mode.
pub const MAX_MESSAGE_SIZE: usize =
    crate::tuning_config::WireProtocolTuning::DEFAULT_MAX_MESSAGE_SIZE;
/// Maximum sender identity length in bytes.
/// Overridable via `[tuning.wire_protocol] max_sender_id_len` in ft.toml.
pub const MAX_SENDER_ID_LEN: usize =
    crate::tuning_config::WireProtocolTuning::DEFAULT_MAX_SENDER_ID_LEN;
/// Default idle window before a sender is considered stale.
pub const DEFAULT_AGENT_STALE_AFTER_MS: i64 = 5 * 60 * 1000;

/// Resolved wire-protocol limits derived from tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireProtocolLimits {
    pub max_message_size: usize,
    pub max_sender_id_len: usize,
}

/// Resolve wire-protocol limits from tuning, falling back to compile-time defaults.
#[must_use]
pub fn resolve_limits(
    tuning: Option<&crate::tuning_config::WireProtocolTuning>,
) -> WireProtocolLimits {
    match tuning {
        Some(tuning) => WireProtocolLimits {
            max_message_size: tuning.max_message_size,
            max_sender_id_len: tuning.max_sender_id_len,
        },
        None => WireProtocolLimits {
            max_message_size: MAX_MESSAGE_SIZE,
            max_sender_id_len: MAX_SENDER_ID_LEN,
        },
    }
}

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
        validate_envelope_protocol(&envelope)?;
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
    #[error("invalid sender identity '{sender}': {reason}")]
    InvalidSender {
        sender: String,
        reason: &'static str,
    },
    #[error("aggregator capacity exceeded: max tracked agents {max}, rejected sender '{sender}'")]
    TooManyAgents { max: usize, sender: String },
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
        if delay.is_nan() || delay.is_sign_negative() {
            return self.max_ms;
        }
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
                content_len: _,
            } => Some(WirePayload::PaneDelta(PaneDelta {
                pane_id: *pane_id,
                seq: *seq,
                // Content is not carried on the bus event; callers that want to
                // emit a real wire delta must fill both fields from storage.
                content: String::new(),
                content_len: 0,
                captured_at_ms: epoch_ms_now(),
            })),

            Event::GapDetected {
                pane_id,
                seq_before,
                seq_after,
                reason,
                detected_at_ms,
            } => Some(WirePayload::Gap(GapNotice {
                pane_id: *pane_id,
                seq_before: *seq_before,
                seq_after: *seq_after,
                reason: reason.clone(),
                detected_at_ms: *detected_at_ms,
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
#[derive(Debug, Clone)]
struct AgentSession {
    /// Last sequence number received from this agent (for ordering/dedup).
    last_seq: u64,
    /// Total messages received from this agent.
    messages_received: u64,
    /// Total duplicates skipped.
    duplicates_skipped: u64,
    /// Local receipt timestamp of the last accepted or duplicate envelope.
    last_seen_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSessionSnapshot {
    last_seq: u64,
    messages_received: u64,
    duplicates_skipped: u64,
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
    stale_after_ms: i64,
}

impl Aggregator {
    /// Create a new aggregator with a maximum number of tracked agents.
    pub fn new(max_agents: usize) -> Self {
        Self::with_stale_after(max_agents, DEFAULT_AGENT_STALE_AFTER_MS)
    }

    /// Create a new aggregator with a custom stale-agent threshold.
    pub fn with_stale_after(max_agents: usize, stale_after_ms: i64) -> Self {
        Self {
            agents: HashMap::new(),
            total_accepted: 0,
            total_rejected: 0,
            max_agents,
            stale_after_ms,
        }
    }

    /// Process a raw JSON wire message. Returns the payload if accepted.
    pub fn ingest(&mut self, bytes: &[u8]) -> Result<IngestResult, WireProtocolError> {
        let envelope = match WireEnvelope::from_json(bytes) {
            Ok(envelope) => envelope,
            Err(err) => {
                self.total_rejected = self.total_rejected.saturating_add(1);
                return Err(err);
            }
        };
        self.ingest_envelope_at(envelope, epoch_ms_now())
    }

    /// Process a decoded envelope. Returns the payload if accepted.
    pub fn ingest_envelope(
        &mut self,
        envelope: WireEnvelope,
    ) -> Result<IngestResult, WireProtocolError> {
        self.ingest_envelope_at(envelope, epoch_ms_now())
    }

    /// Process a decoded envelope using the aggregator host's receipt clock.
    ///
    /// The caller must pass a local receive timestamp, not the sender-reported
    /// `sent_at_ms`. Capacity eviction and stale-session pruning are local
    /// liveness decisions and must not trust remote clocks.
    pub fn ingest_envelope_at(
        &mut self,
        envelope: WireEnvelope,
        received_at_ms: i64,
    ) -> Result<IngestResult, WireProtocolError> {
        if let Err(err) = validate_envelope_protocol(&envelope) {
            self.total_rejected = self.total_rejected.saturating_add(1);
            return Err(err);
        }

        let is_new = !self.agents.contains_key(&envelope.sender);
        if is_new && self.agents.len() >= self.max_agents {
            self.prune_stale_agents(received_at_ms);
        }
        if is_new && self.agents.len() >= self.max_agents {
            self.total_rejected = self.total_rejected.saturating_add(1);
            return Err(WireProtocolError::TooManyAgents {
                max: self.max_agents,
                sender: envelope.sender,
            });
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
        // Use messages_received > 0 to allow seq=0 on first message.
        if session.messages_received > 0 && envelope.seq <= session.last_seq {
            session.duplicates_skipped += 1;
            session.last_seen_ms = session.last_seen_ms.max(received_at_ms);
            return Ok(IngestResult::Duplicate {
                sender: envelope.sender,
                seq: envelope.seq,
            });
        }

        session.last_seq = envelope.seq;
        session.messages_received += 1;
        session.last_seen_ms = session.last_seen_ms.max(received_at_ms);
        self.total_accepted += 1;

        Ok(IngestResult::Accepted(envelope.payload))
    }

    /// Number of unique agents currently tracked.
    #[must_use]
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Remove a tracked sender session explicitly.
    ///
    /// Returns `true` when a session was present and removed.
    pub fn remove_agent(&mut self, sender: &str) -> bool {
        self.agents.remove(sender).is_some()
    }

    #[must_use]
    pub fn agent_session_snapshot(&self, sender: &str) -> Option<AgentSessionSnapshot> {
        self.agents.get(sender).map(|session| AgentSessionSnapshot {
            last_seq: session.last_seq,
            messages_received: session.messages_received,
            duplicates_skipped: session.duplicates_skipped,
            last_seen_ms: session.last_seen_ms,
        })
    }

    pub fn rollback_accepted(&mut self, sender: &str, previous: Option<AgentSessionSnapshot>) {
        match previous {
            Some(previous) => {
                self.agents.insert(
                    sender.to_string(),
                    AgentSession {
                        last_seq: previous.last_seq,
                        messages_received: previous.messages_received,
                        duplicates_skipped: previous.duplicates_skipped,
                        last_seen_ms: previous.last_seen_ms,
                    },
                );
            }
            None => {
                self.agents.remove(sender);
            }
        }
        self.total_accepted = self.total_accepted.saturating_sub(1);
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

    /// Remove tracked senders that have not been seen within `stale_after_ms`.
    ///
    /// Returns the number of removed sender sessions.
    pub fn prune_stale_agents(&mut self, now_ms: i64) -> usize {
        if self.stale_after_ms <= 0 {
            return 0;
        }

        let before = self.agents.len();
        self.agents
            .retain(|_, session| now_ms.saturating_sub(session.last_seen_ms) < self.stale_after_ms);
        before.saturating_sub(self.agents.len())
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
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

fn validate_sender_identity(sender: &str) -> Result<(), WireProtocolError> {
    if sender.trim().is_empty() {
        return Err(WireProtocolError::InvalidSender {
            sender: sender.to_string(),
            reason: "sender must not be empty",
        });
    }
    if sender.len() > MAX_SENDER_ID_LEN {
        return Err(WireProtocolError::InvalidSender {
            sender: sender.to_string(),
            reason: "sender exceeds max length",
        });
    }
    if sender
        .bytes()
        .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')))
    {
        return Err(WireProtocolError::InvalidSender {
            sender: sender.to_string(),
            reason: "sender contains invalid characters",
        });
    }
    Ok(())
}

fn validate_envelope_protocol(envelope: &WireEnvelope) -> Result<(), WireProtocolError> {
    if envelope.version != PROTOCOL_VERSION {
        return Err(WireProtocolError::VersionMismatch {
            expected: PROTOCOL_VERSION,
            got: envelope.version,
        });
    }
    validate_sender_identity(&envelope.sender)?;
    if let WirePayload::PaneDelta(delta) = &envelope.payload {
        if delta.content_len != delta.content.len() {
            return Err(WireProtocolError::InvalidJson(serde_json::Error::custom(
                format!(
                    "PaneDelta content_len ({}) does not match content length ({})",
                    delta.content_len,
                    delta.content.len()
                ),
            )));
        }
    }
    if let WirePayload::Gap(gap) = &envelope.payload {
        if gap.seq_after <= gap.seq_before {
            return Err(WireProtocolError::InvalidJson(serde_json::Error::custom(
                format!(
                    "GapNotice seq_after ({}) must be greater than seq_before ({})",
                    gap.seq_after, gap.seq_before
                ),
            )));
        }
    }
    Ok(())
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

    #[test]
    fn rejects_empty_sender_identity() {
        let mut envelope = WireEnvelope::new(1, "agent", WirePayload::Gap(sample_gap()));
        envelope.sender = "   ".to_string();
        let bytes = envelope.to_json().unwrap();
        let err = WireEnvelope::from_json(&bytes).unwrap_err();
        assert!(matches!(err, WireProtocolError::InvalidSender { .. }));
    }

    #[test]
    fn rejects_sender_identity_with_separator_characters() {
        let mut envelope = WireEnvelope::new(1, "agent", WirePayload::Gap(sample_gap()));
        envelope.sender = "agent:beta".to_string();
        let bytes = envelope.to_json().unwrap();
        let err = WireEnvelope::from_json(&bytes).unwrap_err();
        assert!(matches!(err, WireProtocolError::InvalidSender { .. }));
    }

    #[test]
    fn rejects_pane_delta_content_len_mismatch_with_nonempty_content() {
        let envelope = WireEnvelope::new(
            1,
            "agent",
            WirePayload::PaneDelta(PaneDelta {
                pane_id: 1,
                seq: 1,
                content: "abc".to_string(),
                content_len: 99,
                captured_at_ms: 123,
            }),
        );
        let bytes = envelope.to_json().unwrap();
        let err = WireEnvelope::from_json(&bytes).unwrap_err();
        assert!(matches!(err, WireProtocolError::InvalidJson(_)));
    }

    #[test]
    fn rejects_pane_delta_content_len_mismatch_with_empty_content() {
        let envelope = WireEnvelope::new(
            1,
            "agent",
            WirePayload::PaneDelta(PaneDelta {
                pane_id: 1,
                seq: 1,
                content: String::new(),
                content_len: 1,
                captured_at_ms: 123,
            }),
        );
        let bytes = envelope.to_json().unwrap();
        let err = WireEnvelope::from_json(&bytes).unwrap_err();
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
                assert!(d.content.is_empty());
                assert_eq!(d.content_len, 0);
            }
            other => panic!("expected PaneDelta, got: {other:?}"),
        }
    }

    #[test]
    fn streamer_converts_gap_detected() {
        let mut streamer = AgentStreamer::new("test");
        let event = Event::GapDetected {
            pane_id: 5,
            seq_before: 8,
            seq_after: 12,
            reason: "timeout".into(),
            detected_at_ms: 9876,
        };
        let envelope = streamer.event_to_envelope(&event).unwrap();
        match &envelope.payload {
            WirePayload::Gap(g) => {
                assert_eq!(g.pane_id, 5);
                assert_eq!(g.seq_before, 8);
                assert_eq!(g.seq_after, 12);
                assert_eq!(g.reason, "timeout");
                assert_eq!(g.detected_at_ms, 9876);
            }
            other => panic!("expected Gap, got: {other:?}"),
        }
    }

    #[test]
    fn streamer_preserves_distinct_bounds_for_same_reason_gap_events() {
        let mut streamer = AgentStreamer::new("test");
        let first = Event::GapDetected {
            pane_id: 5,
            seq_before: 8,
            seq_after: 12,
            reason: "timeout".into(),
            detected_at_ms: 1111,
        };
        let second = Event::GapDetected {
            pane_id: 5,
            seq_before: 12,
            seq_after: 20,
            reason: "timeout".into(),
            detected_at_ms: 2222,
        };

        let first_env = streamer.event_to_envelope(&first).unwrap();
        let second_env = streamer.event_to_envelope(&second).unwrap();

        match (&first_env.payload, &second_env.payload) {
            (WirePayload::Gap(first_gap), WirePayload::Gap(second_gap)) => {
                assert_eq!((first_gap.seq_before, first_gap.seq_after), (8, 12));
                assert_eq!((second_gap.seq_before, second_gap.seq_after), (12, 20));
                assert_eq!(first_gap.reason, second_gap.reason);
                assert_ne!(first_gap.detected_at_ms, second_gap.detected_at_ms);
            }
            other => panic!("expected gap payloads, got {other:?}"),
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
        let events = [
            Event::SegmentCaptured {
                pane_id: 1,
                seq: 1,
                content_len: 10,
            },
            Event::GapDetected {
                pane_id: 1,
                seq_before: 4,
                seq_after: 5,
                reason: "test".into(),
                detected_at_ms: 1234,
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
        assert!(matches!(
            result,
            IngestResult::Accepted(WirePayload::Gap(_))
        ));
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
    fn aggregator_remove_agent_frees_capacity_for_new_sender() {
        let mut agg = Aggregator::new(1);
        let first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));

        assert!(matches!(
            agg.ingest_envelope(first).unwrap(),
            IngestResult::Accepted(_)
        ));
        assert!(agg.remove_agent("agent-a"));
        assert!(!agg.remove_agent("missing"));
        assert_eq!(agg.agent_count(), 0);
        assert_eq!(agg.agent_last_seq("agent-a"), None);
        assert!(matches!(
            agg.ingest_envelope(second).unwrap(),
            IngestResult::Accepted(_)
        ));
        assert_eq!(agg.agent_last_seq("agent-b"), Some(1));
    }

    #[test]
    fn aggregator_rejects_malformed_input() {
        let mut agg = Aggregator::new(10);
        let result = agg.ingest(b"not json");
        assert!(result.is_err());
        assert_eq!(agg.total_rejected(), 1);
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
        assert_eq!(agg.total_rejected(), 1);
    }

    #[test]
    fn aggregator_rejected_counter_tracks_multiple_failures() {
        let mut agg = Aggregator::new(10);

        assert!(agg.ingest(b"not json").is_err());
        let huge = vec![b'{'; MAX_MESSAGE_SIZE + 1];
        assert!(matches!(
            agg.ingest(&huge),
            Err(WireProtocolError::MessageTooLarge { .. })
        ));

        assert_eq!(
            agg.total_rejected(),
            2,
            "total_rejected should accumulate malformed/oversize failures"
        );
        assert_eq!(
            agg.total_accepted(),
            0,
            "rejected inputs must not inflate accepted counters"
        );
    }

    #[test]
    fn aggregator_rejects_invalid_sender_identity_and_tracks_rejections() {
        let mut agg = Aggregator::new(10);
        let envelope = WireEnvelope::new(1, "agent:invalid", WirePayload::Gap(sample_gap()));
        let err = agg
            .ingest_envelope(envelope)
            .expect_err("invalid sender identity should be rejected");
        assert!(matches!(err, WireProtocolError::InvalidSender { .. }));
        assert_eq!(agg.total_rejected(), 1);
        assert_eq!(agg.total_accepted(), 0);
        assert_eq!(agg.agent_count(), 0);
    }

    #[test]
    fn aggregator_ingest_envelope_rejects_pane_delta_content_len_mismatch() {
        let mut agg = Aggregator::new(10);
        let envelope = WireEnvelope::new(
            1,
            "agent-valid",
            WirePayload::PaneDelta(PaneDelta {
                pane_id: 7,
                seq: 3,
                content: String::new(),
                content_len: 5,
                captured_at_ms: 123,
            }),
        );
        let err = agg
            .ingest_envelope(envelope)
            .expect_err("decoded envelope path must enforce PaneDelta invariants");
        assert!(matches!(err, WireProtocolError::InvalidJson(_)));
        assert_eq!(agg.total_rejected(), 1);
        assert_eq!(agg.total_accepted(), 0);
    }

    #[test]
    fn aggregator_ingest_envelope_rejects_gap_notice_with_non_increasing_bounds() {
        let mut agg = Aggregator::new(10);
        let envelope = WireEnvelope::new(
            1,
            "agent-valid",
            WirePayload::Gap(GapNotice {
                pane_id: 7,
                seq_before: 8,
                seq_after: 8,
                reason: "invalid-gap".to_string(),
                detected_at_ms: 123,
            }),
        );
        let err = agg
            .ingest_envelope(envelope)
            .expect_err("decoded envelope path must enforce GapNotice invariants");
        assert!(matches!(err, WireProtocolError::InvalidJson(_)));
        assert_eq!(agg.total_rejected(), 1);
        assert_eq!(agg.total_accepted(), 0);
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
                seq_before: 6,
                seq_after: 7,
                reason: "restart".into(),
                detected_at_ms: 4321,
            },
        ];

        for event in &events {
            if let Some(mut envelope) = streamer.event_to_envelope(event) {
                if let WirePayload::PaneDelta(delta) = &mut envelope.payload {
                    delta.content = "streamed segment".to_string();
                    delta.content_len = delta.content.len();
                }
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

    #[test]
    fn aggregator_rejects_new_sender_over_capacity() {
        let mut agg = Aggregator::new(1);

        let first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        assert!(matches!(
            agg.ingest_envelope(first).unwrap(),
            IngestResult::Accepted(_)
        ));

        let second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        let err = agg
            .ingest_envelope(second)
            .expect_err("new sender over capacity");
        assert!(matches!(
            err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(agg.agent_count(), 1);
        assert_eq!(agg.total_accepted(), 1);
        assert_eq!(agg.total_rejected(), 1);
    }

    #[test]
    fn aggregator_accepts_existing_sender_at_capacity() {
        let mut agg = Aggregator::new(1);

        let e1 = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let e2 = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));
        assert!(matches!(
            agg.ingest_envelope(e1).unwrap(),
            IngestResult::Accepted(_)
        ));
        assert!(matches!(
            agg.ingest_envelope(e2).unwrap(),
            IngestResult::Accepted(_)
        ));

        assert_eq!(agg.agent_count(), 1);
        assert_eq!(agg.total_accepted(), 2);
        assert_eq!(agg.total_rejected(), 0);
    }

    #[test]
    fn aggregator_ingest_raw_rejects_new_sender_over_capacity() {
        let mut agg = Aggregator::new(1);

        let first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let first_bytes = first.to_json().expect("serialize");
        assert!(matches!(
            agg.ingest(&first_bytes).unwrap(),
            IngestResult::Accepted(_)
        ));

        let second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        let second_bytes = second.to_json().expect("serialize");
        let err = agg
            .ingest(&second_bytes)
            .expect_err("second sender should be rejected at capacity");
        assert!(matches!(
            err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(agg.total_rejected(), 1);
    }

    #[test]
    fn aggregator_prunes_stale_agents_before_capacity_reject() {
        let mut agg = Aggregator::with_stale_after(1, 50);

        let mut first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        first.sent_at_ms = 50_000;
        assert!(matches!(
            agg.ingest_envelope_at(first, 100).unwrap(),
            IngestResult::Accepted(_)
        ));

        let mut second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        second.sent_at_ms = 50_001;
        assert!(matches!(
            agg.ingest_envelope_at(second, 200).unwrap(),
            IngestResult::Accepted(_)
        ));

        assert_eq!(agg.agent_count(), 1);
        assert_eq!(agg.agent_last_seq("agent-a"), None);
        assert_eq!(agg.agent_last_seq("agent-b"), Some(1));
        assert_eq!(agg.total_rejected(), 0);
    }

    #[test]
    fn aggregator_retains_recent_agents_under_stale_threshold() {
        let mut agg = Aggregator::with_stale_after(1, 200);

        let mut first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        first.sent_at_ms = 50_000;
        assert!(matches!(
            agg.ingest_envelope_at(first, 100).unwrap(),
            IngestResult::Accepted(_)
        ));

        let mut second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        second.sent_at_ms = 1;
        let err = agg
            .ingest_envelope_at(second, 250)
            .expect_err("recent sender should still count against capacity");
        assert!(matches!(
            err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(agg.agent_count(), 1);
        assert_eq!(agg.agent_last_seq("agent-a"), Some(1));
        assert_eq!(agg.total_rejected(), 1);
    }

    #[test]
    fn aggregator_duplicate_refreshes_last_seen_for_stale_pruning() {
        let mut agg = Aggregator::with_stale_after(1, 50);

        let mut first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        first.sent_at_ms = 100;
        assert!(matches!(
            agg.ingest_envelope_at(first, 100).unwrap(),
            IngestResult::Accepted(_)
        ));

        let mut duplicate = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        duplicate.sent_at_ms = 1;
        assert!(matches!(
            agg.ingest_envelope_at(duplicate, 130).unwrap(),
            IngestResult::Duplicate { .. }
        ));

        let mut second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        second.sent_at_ms = 10_000;
        let err = agg
            .ingest_envelope_at(second, 160)
            .expect_err("duplicate should refresh last_seen so sender-a is not stale yet");
        assert!(matches!(
            err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(agg.agent_last_seq("agent-a"), Some(1));
        assert_eq!(agg.agent_last_seq("agent-b"), None);
        assert_eq!(agg.total_rejected(), 1);
    }

    #[test]
    fn aggregator_accepted_envelope_does_not_regress_last_seen_for_stale_pruning() {
        let mut agg = Aggregator::with_stale_after(1, 50);

        let mut first = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        first.sent_at_ms = 100;
        assert!(matches!(
            agg.ingest_envelope_at(first, 100).unwrap(),
            IngestResult::Accepted(_)
        ));

        // Simulate sender clock regression on a new accepted sequence.
        let mut regressed = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));
        regressed.sent_at_ms = 90;
        assert!(matches!(
            agg.ingest_envelope_at(regressed, 140).unwrap(),
            IngestResult::Accepted(_)
        ));

        let mut second = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        second.sent_at_ms = 1_000;
        let err = agg
            .ingest_envelope_at(second, 180)
            .expect_err("accepted seq with regressed timestamp must not make sender-a look stale");
        assert!(matches!(
            err,
            WireProtocolError::TooManyAgents { max: 1, sender: _ }
        ));
        assert_eq!(agg.agent_last_seq("agent-a"), Some(2));
        assert_eq!(agg.agent_last_seq("agent-b"), None);
        assert_eq!(agg.total_rejected(), 1);
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ──────────────────────

    // ── ConnectionState coverage ────────────────────────────

    #[test]
    fn connection_state_debug_clone_copy() {
        let s = ConnectionState::Connected;
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("Connected"));
        let copied = s; // Copy
        let cloned = s; // Clone
        assert_eq!(copied, cloned);
    }

    #[test]
    fn connection_state_serde_roundtrip_all() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting { attempt: 3 },
        ];
        for state in &states {
            let json = serde_json::to_string(state).unwrap();
            let back: ConnectionState = serde_json::from_str(&json).unwrap();
            assert_eq!(*state, back);
        }
    }

    #[test]
    fn connection_state_equality() {
        assert_eq!(ConnectionState::Disconnected, ConnectionState::Disconnected);
        assert_ne!(ConnectionState::Connected, ConnectionState::Disconnected);
        assert_eq!(
            ConnectionState::Reconnecting { attempt: 2 },
            ConnectionState::Reconnecting { attempt: 2 }
        );
        assert_ne!(
            ConnectionState::Reconnecting { attempt: 1 },
            ConnectionState::Reconnecting { attempt: 2 }
        );
    }

    // ── BackoffConfig coverage ──────────────────────────────

    #[test]
    fn backoff_default_values() {
        let b = BackoffConfig::default();
        assert_eq!(b.initial_ms, 500);
        assert_eq!(b.max_ms, 30_000);
        assert!((b.multiplier - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn backoff_debug_clone() {
        let b = BackoffConfig::default();
        let dbg = format!("{:?}", b);
        assert!(dbg.contains("BackoffConfig"));
        let b2 = b.clone();
        assert_eq!(b2.initial_ms, 500);
    }

    #[test]
    fn backoff_delay_attempt_zero() {
        let b = BackoffConfig::default();
        assert_eq!(b.delay_ms(0), 500);
    }

    // ── WireProtocolError coverage ──────────────────────────

    #[test]
    fn wire_error_debug_format() {
        let err = WireProtocolError::MessageTooLarge {
            size: 2_000_000,
            max: MAX_MESSAGE_SIZE,
        };
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("MessageTooLarge"));
    }

    #[test]
    fn wire_error_display_all_variants() {
        let e1 = WireProtocolError::MessageTooLarge {
            size: 999,
            max: 100,
        };
        let d1 = format!("{}", e1);
        assert!(d1.contains("too large"));

        let e2 = WireProtocolError::VersionMismatch {
            expected: 1,
            got: 2,
        };
        let d2 = format!("{}", e2);
        assert!(d2.contains("mismatch"));

        let e3 = WireProtocolError::TooManyAgents {
            max: 5,
            sender: "x".to_string(),
        };
        let d3 = format!("{}", e3);
        assert!(d3.contains("capacity"));
    }

    // ── IngestResult coverage ───────────────────────────────

    #[test]
    fn ingest_result_debug_clone() {
        let r = IngestResult::Duplicate {
            sender: "agent-x".to_string(),
            seq: 42,
        };
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("Duplicate"));
        let r2 = r.clone();
        assert_eq!(r, r2);
    }

    #[test]
    fn ingest_result_accepted_partial_eq() {
        let r1 = IngestResult::Accepted(WirePayload::Gap(sample_gap()));
        let r2 = IngestResult::Accepted(WirePayload::Gap(sample_gap()));
        assert_eq!(r1, r2);
    }

    // ── WireEnvelope coverage ───────────────────────────────

    #[test]
    fn envelope_debug_clone() {
        let e = WireEnvelope::new(1, "agent", WirePayload::Gap(sample_gap()));
        let dbg = format!("{:?}", e);
        assert!(dbg.contains("WireEnvelope"));
        let e2 = e.clone();
        assert_eq!(e, e2);
    }

    // ── AgentStreamer coverage ───────────────────────────────

    #[test]
    fn streamer_messages_dropped_initially_zero() {
        let s = AgentStreamer::new("test");
        assert_eq!(s.messages_dropped(), 0);
        assert_eq!(s.messages_sent(), 0);
    }

    #[test]
    fn streamer_with_backoff_custom() {
        let backoff = BackoffConfig {
            initial_ms: 100,
            max_ms: 5_000,
            multiplier: 1.5,
        };
        let s = AgentStreamer::with_backoff("test", backoff);
        assert_eq!(s.state(), ConnectionState::Disconnected);
        assert_eq!(s.seq(), 0);
    }

    #[test]
    fn streamer_mark_disconnected() {
        let mut s = AgentStreamer::new("test");
        s.mark_connected();
        assert_eq!(s.state(), ConnectionState::Connected);
        s.mark_disconnected();
        assert_eq!(s.state(), ConnectionState::Disconnected);
    }

    // ── Aggregator coverage ─────────────────────────────────

    #[test]
    fn aggregator_agent_count_tracks_senders() {
        let mut agg = Aggregator::new(10);
        assert_eq!(agg.agent_count(), 0);
        let e = WireEnvelope::new(1, "a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope(e);
        assert_eq!(agg.agent_count(), 1);
    }

    #[test]
    fn aggregator_agent_last_seq_nonexistent() {
        let agg = Aggregator::new(10);
        assert_eq!(agg.agent_last_seq("nonexistent"), None);
    }

    // ── Constants coverage ──────────────────────────────────

    #[test]
    fn protocol_constants() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(MAX_MESSAGE_SIZE, 1_048_576);
        assert!(DEFAULT_AGENT_STALE_AFTER_MS > 0);
    }

    // ── WirePayload coverage ────────────────────────────────

    #[test]
    fn payload_debug_clone_all_variants() {
        let payloads = vec![
            WirePayload::PaneMeta(sample_pane_meta()),
            WirePayload::PaneDelta(sample_pane_delta()),
            WirePayload::Gap(sample_gap()),
            WirePayload::Detection(sample_detection()),
            WirePayload::PanesMeta(sample_panes_meta()),
        ];
        for p in &payloads {
            let dbg = format!("{:?}", p);
            assert!(!dbg.is_empty());
            let p2 = p.clone();
            assert_eq!(*p, p2);
        }
    }

    // ── PaneMeta/PaneDelta/GapNotice/DetectionNotice ────────

    #[test]
    fn pane_meta_debug_clone_eq() {
        let pm = sample_pane_meta();
        let dbg = format!("{:?}", pm);
        assert!(dbg.contains("PaneMeta"));
        let pm2 = pm.clone();
        assert_eq!(pm, pm2);
    }

    #[test]
    fn gap_notice_debug_clone_eq() {
        let g = sample_gap();
        let dbg = format!("{:?}", g);
        assert!(dbg.contains("GapNotice"));
        let g2 = g.clone();
        assert_eq!(g, g2);
    }

    #[test]
    fn detection_notice_debug_clone() {
        let d = sample_detection();
        let dbg = format!("{:?}", d);
        assert!(dbg.contains("DetectionNotice"));
        let d2 = d.clone();
        assert_eq!(d, d2);
    }

    // ── Interleaved multi-agent ordering + rollback (wa-nu4.4.3.3) ──

    #[test]
    fn aggregator_rollback_restores_previous_snapshot() {
        let mut agg = Aggregator::new(10);
        let now = 1_000i64;

        // Ingest two messages from agent-a.
        let e1 = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e1, now);
        let snapshot_after_1 = agg.agent_session_snapshot("agent-a");

        let e2 = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e2, now + 10);
        assert_eq!(agg.agent_last_seq("agent-a"), Some(2));
        assert_eq!(agg.total_accepted(), 2);

        // Rollback to snapshot after message 1.
        agg.rollback_accepted("agent-a", snapshot_after_1);
        assert_eq!(agg.agent_last_seq("agent-a"), Some(1));
        assert_eq!(agg.total_accepted(), 1);
    }

    #[test]
    fn aggregator_rollback_to_none_removes_agent() {
        let mut agg = Aggregator::new(10);
        let e1 = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e1, 1000);
        assert_eq!(agg.agent_count(), 1);

        // Rollback to None removes the agent entirely.
        agg.rollback_accepted("agent-a", None);
        assert_eq!(agg.agent_count(), 0);
        assert_eq!(agg.agent_last_seq("agent-a"), None);
        assert_eq!(agg.total_accepted(), 0);
    }

    #[test]
    fn aggregator_rollback_then_reingest_works() {
        let mut agg = Aggregator::new(10);
        let now = 1_000i64;

        let e1 = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e1, now);
        let snap = agg.agent_session_snapshot("agent-a");

        let e2 = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e2, now + 10);

        // Rollback seq 2.
        agg.rollback_accepted("agent-a", snap);

        // Re-ingest with seq 2 should succeed again.
        let e2_retry = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));
        let result = agg.ingest_envelope_at(e2_retry, now + 20).unwrap();
        assert!(matches!(result, IngestResult::Accepted(_)));
        assert_eq!(agg.agent_last_seq("agent-a"), Some(2));
    }

    #[test]
    fn aggregator_interleaved_multi_agent_preserves_per_agent_ordering() {
        let mut agg = Aggregator::new(64);
        let now = 1_000i64;

        // Simulate 5 agents each sending 20 messages, interleaved round-robin.
        let agent_count = 5;
        let msgs_per_agent = 20u64;
        let mut expected_seqs = vec![0u64; agent_count];

        for msg_idx in 0..msgs_per_agent {
            for (agent_idx, expected_seq) in expected_seqs.iter_mut().enumerate() {
                let sender = format!("agent-{agent_idx}");
                let seq = msg_idx + 1; // 1-based seq
                let envelope = WireEnvelope::new(
                    seq,
                    &sender,
                    WirePayload::Gap(sample_gap()),
                );
                let result = agg.ingest_envelope_at(envelope, now + (msg_idx as i64) * 100).unwrap();
                assert!(
                    matches!(result, IngestResult::Accepted(_)),
                    "agent-{agent_idx} seq {seq} should be accepted"
                );
                *expected_seq = seq;
            }
        }

        // Verify each agent's last_seq is correct.
        for agent_idx in 0..agent_count {
            let sender = format!("agent-{agent_idx}");
            assert_eq!(agg.agent_last_seq(&sender), Some(msgs_per_agent));
        }
        assert_eq!(agg.agent_count(), agent_count);
        assert_eq!(agg.total_accepted(), (agent_count as u64) * msgs_per_agent);
        assert_eq!(agg.total_rejected(), 0);
    }

    #[test]
    fn aggregator_interleaved_with_gaps_and_duplicates() {
        let mut agg = Aggregator::new(10);
        let now = 1_000i64;

        // Agent A: 1, 3, 5 (gaps between seqs)
        // Agent B: 1, 1, 2 (duplicate at seq 1)
        // Agent C: 10 (high starting seq)
        let messages: Vec<(&str, u64, bool)> = vec![
            ("a", 1, true),   // accepted
            ("b", 1, true),   // accepted
            ("a", 3, true),   // accepted (gap is ok, just monotonic)
            ("b", 1, false),  // duplicate
            ("c", 10, true),  // accepted (high seq ok for first msg)
            ("a", 5, true),   // accepted
            ("b", 2, true),   // accepted
            ("a", 3, false),  // duplicate (already seen 5)
            ("c", 9, false),  // duplicate (already seen 10)
        ];

        for (sender, seq, should_accept) in messages {
            let envelope = WireEnvelope::new(
                seq,
                sender,
                WirePayload::Gap(sample_gap()),
            );
            let result = agg.ingest_envelope_at(envelope, now).unwrap();
            if should_accept {
                assert!(
                    matches!(result, IngestResult::Accepted(_)),
                    "{sender} seq {seq} should be accepted"
                );
            } else {
                assert!(
                    matches!(result, IngestResult::Duplicate { .. }),
                    "{sender} seq {seq} should be duplicate"
                );
            }
        }

        assert_eq!(agg.agent_last_seq("a"), Some(5));
        assert_eq!(agg.agent_last_seq("b"), Some(2));
        assert_eq!(agg.agent_last_seq("c"), Some(10));
        assert_eq!(agg.total_accepted(), 6);
    }

    #[test]
    fn aggregator_capacity_eviction_interleaved_with_stale_pruning() {
        // Capacity=2, stale_after=100ms. Three agents compete for 2 slots.
        // prune_stale_agents retains sessions where (now - last_seen) < stale_after.
        let mut agg = Aggregator::with_stale_after(2, 100);

        // Agent A at t=0
        let e_a = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        assert!(matches!(agg.ingest_envelope_at(e_a, 0).unwrap(), IngestResult::Accepted(_)));

        // Agent B at t=60 (will still be fresh at t=150 since 150-60=90 < 100)
        let e_b = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        assert!(matches!(agg.ingest_envelope_at(e_b, 60).unwrap(), IngestResult::Accepted(_)));

        // Agent C at t=150 — agent-a is stale (last_seen=0, 150-0=150 >= 100)
        // agent-b is fresh (last_seen=60, 150-60=90 < 100)
        // Prune agent-a, accept agent-c.
        let e_c = WireEnvelope::new(1, "agent-c", WirePayload::Gap(sample_gap()));
        assert!(matches!(agg.ingest_envelope_at(e_c, 150).unwrap(), IngestResult::Accepted(_)));

        assert_eq!(agg.agent_count(), 2);
        assert_eq!(agg.agent_last_seq("agent-a"), None);
        assert_eq!(agg.agent_last_seq("agent-b"), Some(1));
        assert_eq!(agg.agent_last_seq("agent-c"), Some(1));
    }

    #[test]
    fn aggregator_snapshot_roundtrip_preserves_all_fields() {
        let mut agg = Aggregator::new(10);
        let now = 1_000i64;

        // Send 3 messages, 1 duplicate.
        let e1 = WireEnvelope::new(1, "agent-snap", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e1, now);
        let e2 = WireEnvelope::new(2, "agent-snap", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(e2, now + 10);
        let dup = WireEnvelope::new(1, "agent-snap", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(dup, now + 20);

        let snap = agg.agent_session_snapshot("agent-snap").unwrap();

        // Wipe and restore from snapshot.
        agg.rollback_accepted("agent-snap", None);
        assert_eq!(agg.agent_count(), 0);

        agg.rollback_accepted("agent-snap", Some(snap));
        // After restore, agent should have seq=2 and next ingest of seq=2 should be dup.
        let e2_again = WireEnvelope::new(2, "agent-snap", WirePayload::Gap(sample_gap()));
        let result = agg.ingest_envelope_at(e2_again, now + 30).unwrap();
        assert!(matches!(result, IngestResult::Duplicate { .. }));

        // seq=3 should be accepted.
        let e3 = WireEnvelope::new(3, "agent-snap", WirePayload::Gap(sample_gap()));
        let result = agg.ingest_envelope_at(e3, now + 40).unwrap();
        assert!(matches!(result, IngestResult::Accepted(_)));
    }

    #[test]
    fn aggregator_rollback_does_not_affect_other_agents() {
        let mut agg = Aggregator::new(10);
        let now = 1_000i64;

        let ea = WireEnvelope::new(1, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(ea, now);
        let eb = WireEnvelope::new(1, "agent-b", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(eb, now);

        let snap_a = agg.agent_session_snapshot("agent-a");

        let ea2 = WireEnvelope::new(2, "agent-a", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(ea2, now + 10);
        let eb2 = WireEnvelope::new(2, "agent-b", WirePayload::Gap(sample_gap()));
        let _ = agg.ingest_envelope_at(eb2, now + 10);

        // Rollback only agent-a.
        agg.rollback_accepted("agent-a", snap_a);

        // agent-a rolled back to seq=1.
        assert_eq!(agg.agent_last_seq("agent-a"), Some(1));
        // agent-b unaffected, still at seq=2.
        assert_eq!(agg.agent_last_seq("agent-b"), Some(2));
    }

    #[test]
    fn aggregator_concurrent_style_burst_from_many_agents() {
        // Simulate a burst: 20 agents each send 50 messages as fast as possible.
        // Verify no cross-contamination between agent state.
        let mut agg = Aggregator::new(64);
        let now = 1_000i64;
        let agent_count = 20usize;
        let msgs = 50u64;

        for seq in 1..=msgs {
            for a in 0..agent_count {
                let sender = format!("agent-{a}");
                let envelope = WireEnvelope::new(
                    seq,
                    &sender,
                    WirePayload::PaneDelta(PaneDelta {
                        pane_id: a as u64,
                        seq,
                        content: format!("data-{a}-{seq}"),
                        content_len: format!("data-{a}-{seq}").len(),
                        captured_at_ms: now,
                    }),
                );
                let result = agg.ingest_envelope_at(envelope, now + seq as i64).unwrap();
                assert!(matches!(result, IngestResult::Accepted(_)));
            }
        }

        assert_eq!(agg.agent_count(), agent_count);
        assert_eq!(agg.total_accepted(), (agent_count as u64) * msgs);
        for a in 0..agent_count {
            assert_eq!(
                agg.agent_last_seq(&format!("agent-{a}")),
                Some(msgs),
                "agent-{a} should have last_seq={msgs}"
            );
        }
    }
}
