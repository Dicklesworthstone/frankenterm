//! Event bus for detections and signals
//!
//! Provides bounded broadcast channels and fanout for system events.
//!
//! # Architecture
//!
//! The event bus uses runtime-compat broadcast channels for multi-consumer fanout:
//! - Single producer publishes events via `EventBus::publish()`
//! - Subscribers can listen to all events or specific channels (delta/detection/signal)
//! - Bounded capacity provides backpressure (slow consumers get lagged)
//!
//! # Example
//!
//! ```no_run
//! use frankenterm_core::events::{EventBus, Event};
//!
//! fn main() {
//!     let runtime = frankenterm_core::runtime_async::RuntimeBuilder::current_thread()
//!         .enable_all()
//!         .build()
//!         .expect("build runtime");
//!     use frankenterm_core::runtime_async::CompatRuntime;
//!     runtime.block_on(async {
//!         let bus = EventBus::new(1000);
//!         let mut subscriber = bus.subscribe();
//!
//!         // Publish events
//!         bus.publish(Event::PaneDiscovered {
//!             pane_id: 1,
//!             domain: "local".to_string(),
//!             title: "shell".to_string(),
//!         });
//!
//!         // Receive events
//!         while let Ok(event) = subscriber.recv().await {
//!             println!("Got event: {:?}", event);
//!         }
//!     });
//! }
//! ```

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::future::{Either, select};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::events_dedup_cuckoo::{CuckooDedupVerdict, EventCuckooDedup, EventCuckooDedupSnapshot};
use crate::patterns::Detection;
use crate::policy::Redactor;
use crate::runtime_async::broadcast;

/// Payload for user-var events received via IPC from shell hooks.
///
/// WezTerm allows setting user-defined variables via OSC 1337, which
/// shell hooks use to signal events like command start/end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserVarPayload {
    /// Raw value (typically base64-encoded JSON)
    pub value: String,
    /// Decoded event type, if parsing succeeded
    pub event_type: Option<String>,
    /// Decoded event data, if parsing succeeded
    pub event_data: Option<serde_json::Value>,
}

impl UserVarPayload {
    /// Attempt to decode the value as base64-encoded JSON.
    ///
    /// # Arguments
    /// * `value` - The raw value string (typically base64-encoded JSON)
    /// * `lenient` - If true, returns Ok with partial data on decode failures
    ///
    /// # Errors
    /// Returns `UserVarError::ParseFailed` if decoding fails and `lenient` is false.
    pub fn decode(value: &str, lenient: bool) -> Result<Self, UserVarError> {
        use base64::Engine;

        let mut payload = Self {
            value: value.to_string(),
            event_type: None,
            event_data: None,
        };

        // Try to decode as base64
        match base64::engine::general_purpose::STANDARD.decode(value) {
            Ok(bytes) => {
                match String::from_utf8(bytes) {
                    Ok(json_str) => {
                        match serde_json::from_str::<serde_json::Value>(&json_str) {
                            Ok(data) => {
                                payload.event_type =
                                    data.get("type").and_then(|v| v.as_str()).map(String::from);
                                payload.event_data = Some(data);
                            }
                            Err(e) if !lenient => {
                                return Err(UserVarError::ParseFailed(format!(
                                    "invalid JSON: {e}"
                                )));
                            }
                            Err(_) => {} // lenient mode - continue with partial data
                        }
                    }
                    Err(e) if !lenient => {
                        return Err(UserVarError::ParseFailed(format!("invalid UTF-8: {e}")));
                    }
                    Err(_) => {} // lenient mode - continue with raw value
                }
            }
            Err(e) if !lenient => {
                return Err(UserVarError::ParseFailed(format!("invalid base64: {e}")));
            }
            Err(_) => {} // lenient mode - continue with raw value
        }

        Ok(payload)
    }
}

/// Errors that can occur when processing user-var events.
#[derive(Debug, Clone, thiserror::Error)]
pub enum UserVarError {
    /// Watcher daemon is not running
    #[error("watcher daemon is not running (socket: {socket_path})")]
    WatcherNotRunning {
        /// Path to the IPC socket that wasn't found
        socket_path: String,
    },

    /// Failed to send event to watcher via IPC
    #[error("failed to send event via IPC: {message}")]
    IpcSendFailed {
        /// Error message describing what failed
        message: String,
    },

    /// Failed to parse user-var payload
    #[error("failed to parse user-var payload: {0}")]
    ParseFailed(String),
}

/// Event types that flow through the system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// New segment captured from a pane
    SegmentCaptured {
        pane_id: u64,
        seq: u64,
        content_len: usize,
    },

    /// Gap detected in capture stream
    GapDetected {
        pane_id: u64,
        seq_before: u64,
        seq_after: u64,
        reason: String,
        detected_at_ms: i64,
    },

    /// Pattern detected
    PatternDetected {
        pane_id: u64,
        /// Stable pane UUID (if available)
        pane_uuid: Option<String>,
        detection: Detection,
        /// Storage event ID (if persisted), for marking as handled by workflows
        event_id: Option<i64>,
    },

    /// Pane discovered
    PaneDiscovered {
        pane_id: u64,
        domain: String,
        title: String,
    },

    /// Pane disappeared
    PaneDisappeared { pane_id: u64 },

    /// Workflow started
    WorkflowStarted {
        workflow_id: String,
        workflow_name: String,
        pane_id: u64,
    },

    /// Workflow step completed
    WorkflowStep {
        workflow_id: String,
        step_name: String,
        result: String,
    },

    /// Workflow completed
    WorkflowCompleted {
        workflow_id: String,
        success: bool,
        reason: Option<String>,
    },

    /// User-var event received via IPC from shell hook
    UserVarReceived {
        pane_id: u64,
        /// Variable name (e.g., "FT_EVENT")
        name: String,
        payload: UserVarPayload,
    },
    // NOTE: StatusUpdateReceived was removed in v0.2.0 to eliminate Lua performance bottleneck.
    // Alt-screen detection is now handled via escape sequence parsing (see screen_state.rs).
    // Pane metadata (title, dimensions, cursor) is obtained via `wezterm cli list`.
}

impl Event {
    /// Returns the event type name for logging/metrics
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::SegmentCaptured { .. } => "segment_captured",
            Self::GapDetected { .. } => "gap_detected",
            Self::PatternDetected { .. } => "pattern_detected",
            Self::PaneDiscovered { .. } => "pane_discovered",
            Self::PaneDisappeared { .. } => "pane_disappeared",
            Self::WorkflowStarted { .. } => "workflow_started",
            Self::WorkflowStep { .. } => "workflow_step",
            Self::WorkflowCompleted { .. } => "workflow_completed",
            Self::UserVarReceived { .. } => "user_var_received",
        }
    }

    /// Returns the pane_id if this event is associated with a pane
    #[must_use]
    pub fn pane_id(&self) -> Option<u64> {
        match self {
            Self::SegmentCaptured { pane_id, .. }
            | Self::GapDetected { pane_id, .. }
            | Self::PatternDetected { pane_id, .. }
            | Self::PaneDiscovered { pane_id, .. }
            | Self::PaneDisappeared { pane_id }
            | Self::WorkflowStarted { pane_id, .. }
            | Self::UserVarReceived { pane_id, .. } => Some(*pane_id),
            Self::WorkflowStep { .. } | Self::WorkflowCompleted { .. } => None,
        }
    }
}

// =============================================================================
// Event Causality Clocks
// =============================================================================

const DEFAULT_EVENT_CAUSALITY_NODE_ID: &str = "local";

/// Causal ordering relation between two vector clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelation {
    /// Both clocks contain the same causal frontier.
    Equal,
    /// The left clock happened before the right clock.
    Before,
    /// The left clock happened after the right clock.
    After,
    /// Neither clock dominates the other.
    Concurrent,
}

/// Total-order Lamport stamp for distributed event sorting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LamportStamp {
    /// Monotonic logical counter.
    pub counter: u64,
    /// Stable process/node identifier used as the deterministic tie breaker.
    pub node_id: String,
}

impl LamportStamp {
    /// Create a Lamport stamp.
    #[must_use]
    pub fn new(counter: u64, node_id: impl Into<String>) -> Self {
        Self {
            counter,
            node_id: node_id.into(),
        }
    }
}

/// Sparse vector clock for cross-node happens-before checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock {
    /// Per-node logical counters. `BTreeMap` keeps snapshots deterministic.
    pub entries: BTreeMap<String, u64>,
}

impl VectorClock {
    /// Create an empty vector clock.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get a node's counter, treating missing nodes as zero.
    #[must_use]
    pub fn get(&self, node_id: &str) -> u64 {
        self.entries.get(node_id).copied().unwrap_or(0)
    }

    /// Increment one node and return the new counter.
    pub fn increment(&mut self, node_id: impl Into<String>) -> u64 {
        let entry = self.entries.entry(node_id.into()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    /// Merge another vector clock using pointwise maximum.
    pub fn merge(&mut self, other: &Self) {
        for (node_id, counter) in &other.entries {
            let entry = self.entries.entry(node_id.clone()).or_insert(0);
            *entry = (*entry).max(*counter);
        }
    }

    /// Compare this clock to another clock.
    #[must_use]
    pub fn relation_to(&self, other: &Self) -> CausalRelation {
        let mut self_less = false;
        let mut other_less = false;

        for node_id in self.entries.keys().chain(other.entries.keys()) {
            let left = self.get(node_id);
            let right = other.get(node_id);
            self_less |= left < right;
            other_less |= right < left;
            if self_less && other_less {
                return CausalRelation::Concurrent;
            }
        }

        match (self_less, other_less) {
            (false, false) => CausalRelation::Equal,
            (true, false) => CausalRelation::Before,
            (false, true) => CausalRelation::After,
            (true, true) => CausalRelation::Concurrent,
        }
    }

    /// Number of nodes represented in the frontier.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.entries.len()
    }
}

/// Hybrid logical clock stamp: wall time for locality, logical tie breaker for causality.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HybridLogicalStamp {
    /// Milliseconds since Unix epoch.
    pub wall_time_ms: u64,
    /// Logical counter for equal or regressing wall-clock observations.
    pub logical: u64,
    /// Stable process/node identifier used as the deterministic tie breaker.
    pub node_id: String,
}

impl HybridLogicalStamp {
    /// Create an HLC stamp.
    #[must_use]
    pub fn new(wall_time_ms: u64, logical: u64, node_id: impl Into<String>) -> Self {
        Self {
            wall_time_ms,
            logical,
            node_id: node_id.into(),
        }
    }
}

/// Complete causality stamp for an event boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCausalityStamp {
    /// Lamport total-order stamp.
    pub lamport: LamportStamp,
    /// Vector-clock happens-before frontier.
    pub vector: VectorClock,
    /// Hybrid logical clock stamp.
    pub hybrid: HybridLogicalStamp,
}

/// Serializable event-bus causality snapshot for doctor/status surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCausalitySnapshot {
    /// Local node identifier.
    pub node_id: String,
    /// Current Lamport counter.
    pub lamport_counter: u64,
    /// Number of nodes represented in the vector clock.
    pub vector_nodes: usize,
    /// Current hybrid logical clock wall component.
    pub hybrid_wall_time_ms: u64,
    /// Current hybrid logical clock logical component.
    pub hybrid_logical: u64,
}

/// Local event-causality clock used by the event bus.
#[derive(Debug, Clone)]
pub struct EventCausalityClock {
    node_id: String,
    lamport_counter: u64,
    vector: VectorClock,
    hybrid_wall_time_ms: u64,
    hybrid_logical: u64,
}

impl EventCausalityClock {
    /// Create a clock for one process/node.
    #[must_use]
    pub fn new(node_id: impl Into<String>) -> Self {
        Self {
            node_id: node_id.into(),
            lamport_counter: 0,
            vector: VectorClock::new(),
            hybrid_wall_time_ms: 0,
            hybrid_logical: 0,
        }
    }

    /// Record a local event at the provided physical time.
    pub fn record_local_event(&mut self, wall_time_ms: u64) -> EventCausalityStamp {
        self.lamport_counter = self.lamport_counter.saturating_add(1);
        self.vector.increment(self.node_id.clone());
        self.advance_hybrid_for_local(wall_time_ms);
        self.stamp()
    }

    /// Merge a remote stamp and record the receive event.
    pub fn observe_remote(
        &mut self,
        remote: &EventCausalityStamp,
        wall_time_ms: u64,
    ) -> EventCausalityStamp {
        self.lamport_counter = self
            .lamport_counter
            .max(remote.lamport.counter)
            .saturating_add(1);
        self.vector.merge(&remote.vector);
        self.vector.increment(self.node_id.clone());
        self.advance_hybrid_for_receive(
            remote.hybrid.wall_time_ms,
            remote.hybrid.logical,
            wall_time_ms,
        );
        self.stamp()
    }

    /// Snapshot the current clock state.
    #[must_use]
    pub fn snapshot(&self) -> EventCausalitySnapshot {
        EventCausalitySnapshot {
            node_id: self.node_id.clone(),
            lamport_counter: self.lamport_counter,
            vector_nodes: self.vector.node_count(),
            hybrid_wall_time_ms: self.hybrid_wall_time_ms,
            hybrid_logical: self.hybrid_logical,
        }
    }

    fn advance_hybrid_for_local(&mut self, wall_time_ms: u64) {
        if wall_time_ms > self.hybrid_wall_time_ms {
            self.hybrid_wall_time_ms = wall_time_ms;
            self.hybrid_logical = 0;
        } else {
            self.hybrid_logical = self.hybrid_logical.saturating_add(1);
        }
    }

    fn advance_hybrid_for_receive(
        &mut self,
        remote_wall_time_ms: u64,
        remote_logical: u64,
        wall_time_ms: u64,
    ) {
        let local_wall_time_ms = self.hybrid_wall_time_ms;
        let next_wall_time_ms = wall_time_ms
            .max(local_wall_time_ms)
            .max(remote_wall_time_ms);
        let next_logical = if next_wall_time_ms == local_wall_time_ms
            && next_wall_time_ms == remote_wall_time_ms
        {
            self.hybrid_logical.max(remote_logical).saturating_add(1)
        } else if next_wall_time_ms == local_wall_time_ms {
            self.hybrid_logical.saturating_add(1)
        } else if next_wall_time_ms == remote_wall_time_ms {
            remote_logical.saturating_add(1)
        } else {
            0
        };

        self.hybrid_wall_time_ms = next_wall_time_ms;
        self.hybrid_logical = next_logical;
    }

    fn stamp(&self) -> EventCausalityStamp {
        EventCausalityStamp {
            lamport: LamportStamp::new(self.lamport_counter, self.node_id.clone()),
            vector: self.vector.clone(),
            hybrid: HybridLogicalStamp::new(
                self.hybrid_wall_time_ms,
                self.hybrid_logical,
                self.node_id.clone(),
            ),
        }
    }
}

impl Default for EventCausalityClock {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAUSALITY_NODE_ID)
    }
}

fn current_unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// =============================================================================
// Event Identity (dedupe/cooldown/mute key)
// =============================================================================

const IDENTITY_KEY_VERSION: &str = "v1";
const IDENTITY_MAX_VALUE_LEN: usize = 120;

/// Build a deterministic identity key for a detection event.
///
/// The key is based on rule_id + event_type + pane_uuid (or pane_id fallback),
/// plus a redacted, bounded projection of extracted fields. The final key is
/// a SHA-256 hash to avoid leaking sensitive values.
#[must_use]
pub fn event_identity_key(detection: &Detection, pane_id: u64, pane_uuid: Option<&str>) -> String {
    let redactor = Redactor::new();
    let mut parts: Vec<String> = Vec::new();
    parts.push(IDENTITY_KEY_VERSION.to_string());
    parts.push(detection.rule_id.clone());
    parts.push(detection.event_type.clone());
    parts.push(pane_uuid.map_or_else(|| format!("pane:{pane_id}"), |uuid| uuid.to_string()));

    if let Some(extracted) = normalized_extracted(&detection.extracted, &redactor) {
        parts.push(extracted);
    }

    let joined = parts.join("|");
    let digest = Sha256::digest(joined.as_bytes());
    format!("evt:{}", hex_encode(&digest))
}

fn normalized_extracted(extracted: &serde_json::Value, redactor: &Redactor) -> Option<String> {
    let obj = extracted.as_object()?;
    if obj.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = Vec::new();
    let mut entries: Vec<(&str, &serde_json::Value)> = obj
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    entries.sort_by_key(|(left, _)| *left);

    for (key, value) in entries {
        let mut rendered = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => "null".to_string(),
            // [review] Nested Object / Array values: canonicalize
            // key ordering recursively before serializing. Without
            // this, a nested `{"context": {"a": 1, "b": 2}}` vs
            // `{"context": {"b": 2, "a": 1}}` produce DIFFERENT
            // serde_json::to_string outputs (serde_json::Map is
            // IndexMap — preserves insertion order), which defeats
            // the top-level sort introduced in 765743d5: the two
            // logically-equivalent events still hash to different
            // identity keys and dedup-to-two. Canonicalize on a
            // clone so the caller's source is untouched.
            _ => {
                let mut canonical = value.clone();
                canonicalize_json_value(&mut canonical);
                serde_json::to_string(&canonical).unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "event value JSON serialization failed");
                    String::new()
                })
            }
        };

        if rendered.is_empty() {
            continue;
        }

        rendered = redactor.redact(&rendered);
        if rendered.len() > IDENTITY_MAX_VALUE_LEN {
            truncate_to_char_boundary(&mut rendered, IDENTITY_MAX_VALUE_LEN);
        }

        parts.push(format!("{key}={rendered}"));
    }

    if parts.is_empty() {
        return None;
    }

    parts.sort();
    Some(parts.join(","))
}

fn truncate_to_char_boundary(value: &mut String, max_len: usize) {
    if value.len() <= max_len {
        return;
    }
    let mut boundary = max_len;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// Recursively canonicalize a `serde_json::Value` so that any Object
/// (at any nesting depth) has its keys in ASCII-lexicographic order.
/// Arrays stay in their original order (arrays are ordered in JSON).
///
/// Used by `normalized_extracted` to prevent nested-object insertion
/// order from leaking into event identity hashes — the same defect
/// that 765743d5 fixed at the top level, but propagated recursively.
fn canonicalize_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(obj) => {
            // serde_json::Map is an IndexMap by default (preserves
            // insertion order). Collect, sort by key, then rebuild.
            // Recurse into each value first so nested Maps are
            // canonicalized before their parent is rebuilt.
            let mut entries: Vec<(String, serde_json::Value)> =
                obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (_, v) in &mut entries {
                canonicalize_json_value(v);
            }
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            obj.clear();
            for (k, v) in entries {
                obj.insert(k, v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                canonicalize_json_value(v);
            }
        }
        _ => {}
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Metrics for monitoring event bus health
#[derive(Debug, Default)]
pub struct EventBusMetrics {
    /// Total events published since bus creation
    pub events_published: AtomicU64,
    /// Events published that had no subscribers
    pub events_dropped_no_subscribers: AtomicU64,
    /// br-ft-8cyii: events dropped by the cuckoo-dedup gate at
    /// `EventBus::publish` (the early-return path when
    /// `is_duplicate_delta_event` returns true). Without this
    /// counter, operators querying MetricsSnapshot can answer
    /// 'how many events went through the bus' but cannot answer
    /// 'how many were filtered as duplicates'.
    ///
    /// Same shape as ft-luav8 (record_mcp_audit silent-failure
    /// counter): silent state loss + observable counter.
    ///
    /// ## br-ft-e3wwx — FPR conflation
    ///
    /// **This counter does NOT distinguish true duplicates from
    /// cuckoo false positives.** The cuckoo filter
    /// `is_duplicate_delta_event` returns
    /// `CuckooDedupVerdict::PossibleDuplicate` for two
    /// indistinguishable cases:
    ///
    /// 1. **True duplicate**: same `(pane_id, seq, content_len)`
    ///    tuple seen twice (the intended drop class).
    /// 2. **Cuckoo false positive**: a *distinct* tuple whose
    ///    fingerprint collides with a previously-seen key. The
    ///    documented FPR ceiling is **≤ 5%** of distinct events,
    ///    pinned by
    ///    `delta_event_bus_cuckoo_false_positive_rate_stays_below_five_percent`
    ///    at events.rs:~2445.
    ///
    /// **Operator interpretation**:
    /// - `events_dropped_dedup` is an **upper bound** on intended
    ///   dedup work and a **lower bound** on the FPR-conflated
    ///   delivery loss.
    /// - True duplicates ≥ `events_dropped_dedup × (1 - FPR)`
    ///   ≥ `events_dropped_dedup × 0.95` (using the 5% ceiling).
    /// - FPR-induced data loss ≤ `events_dropped_dedup × FPR`
    ///   ≤ `events_dropped_dedup × 0.05`.
    ///
    /// The forensic invariant from ft-2z16v
    /// (`events_published == events_delivered +
    /// events_dropped_no_subscribers + events_dropped_dedup`)
    /// still holds in counts, but `events_delivered` undercounts
    /// true delivery by up to 5% of distinct events under the FPR
    /// ceiling, and `events_dropped_dedup` overcounts true dedup
    /// by the same amount.
    ///
    /// **Why this is unfixed in the runtime**: distinguishing
    /// the two cases requires either a sidecar exact dedup (a
    /// bounded HashSet of recent keys) or a statistical FPR
    /// estimator. Both are larger than this docstring fix; ft-e3wwx
    /// proposes either path. Until then, operators reading this
    /// counter must apply the ≤ 5% ceiling to interpret it.
    ///
    /// Pairs with ft-tpdl5 (cuckoo capacity exhaustion) — together
    /// they make up the cuckoo-filter observability backlog.
    pub events_dropped_dedup: AtomicU64,
    /// br-ft-2z16v: count of distinct events that reached at least
    /// one subscriber. NOT the fanout — incremented by exactly 1
    /// per published event whose `delivered` tally was > 0.
    ///
    /// The pre-fix invariant
    /// `events_published == delivered + dropped_no_subscribers + dropped_dedup`
    /// was numerically wrong because `delivered` is the SUM of
    /// fanout counts across all_sender AND the routed sender —
    /// one event with N all-subscribers and M delta-subscribers
    /// contributes N+M to delivered. This counter restores the
    /// closed forensic invariant in event-units, not fanout-units:
    ///
    /// ```text
    /// events_published == events_delivered
    ///                      + events_dropped_no_subscribers
    ///                      + events_dropped_dedup
    /// ```
    ///
    /// Both sides count distinct events; the identity holds for
    /// every subscriber topology.
    pub events_delivered: AtomicU64,
    /// Number of currently active subscribers
    pub active_subscribers: AtomicU64,
    /// **Fanout-weighted** subscriber-missed-message + send-failure
    /// counter.
    ///
    /// br-ft-lb5x7: the legacy field name reads as if this counts
    /// "events lagged" (a per-event quantity), but the runtime
    /// actually mixes three distinct semantics into one slot:
    ///
    /// 1. **`recv_cx` Lagged arm** (`events.rs::Subscriber::recv_cx`):
    ///    `fetch_add(missed_count)` per subscriber that observes
    ///    a `broadcast::RecvError::Lagged(n)`. With N subscribers
    ///    all lagging on the same M events, this contributes
    ///    `N × M`, not `M` — fanout-weighted.
    /// 2. **`try_recv` Lagged arm**
    ///    (`events.rs::Subscriber::try_recv`): same `fetch_add(n)`
    ///    shape as #1.
    /// 3. **`send_routed` broadcast_send Err arm**
    ///    (`events.rs::EventBusInner::send_routed`):
    ///    `fetch_add(1)` per failed send when active subscribers
    ///    exist. asupersync's `broadcast_send` returns Err only
    ///    when the channel has zero receivers, but the call site
    ///    short-circuits on `broadcast_receiver_count > 0` so
    ///    this can fire on race-windows where receivers drop
    ///    between the count check and the send. Different
    ///    semantic from #1/#2 (send-side, not receive-side).
    ///
    /// **Operator-facing meaning:** treat this counter as a
    /// "subscriber-missed-message + send-failure event count"
    /// rather than a "distinct-events-lagged count". Multiply by
    /// the average subscriber count for a rough fanout-weighted
    /// upper bound; divide by the active-subscriber count for a
    /// rough per-event lower bound. The exact distinct-events-
    /// lagged figure is recoverable from
    /// `ChannelLagTracker::queued_len` over time but is not yet
    /// surfaced as a counter.
    ///
    /// Operationally similar to ft-2z16v (events_dropped_dedup
    /// invariant doc-correctness).
    ///
    /// ## Forensic invariant (semi-formal)
    ///
    /// ```text
    /// subscriber_lag_events ==
    ///     Σ_subscribers (per-subscriber missed_count from Lagged events)
    ///   + send_routed_err_with_active_subscribers_count
    /// ```
    ///
    /// The first term dominates in practice (N subscribers lagging
    /// on M events → N × M); the second term is a small constant
    /// trickle from race-window send failures.
    pub subscriber_lag_events: AtomicU64,

    /// br-ft-skec1: cumulative count of times an internal
    /// `EventBus` Mutex was poisoned and the runtime fell through
    /// with a default value (silent degradation).
    ///
    /// Mutex poisoning happens when a thread holding the lock
    /// panics. Post-poison, every subsequent `lock()` returns
    /// `Err(PoisonError)`. The bus catches the error and falls
    /// through with a default — events keep flowing, but
    /// **dedup, causality tracking, and lag-timing are all silently
    /// disabled** for the rest of the process. This counter
    /// surfaces that state-loss to operators.
    ///
    /// Six sources contribute to this counter:
    /// 1. `is_duplicate_delta_event` poison → dedup disabled
    ///    (cuckoo bypassed; duplicates leak through).
    /// 2. `delta_dedup_snapshot` poison → snapshot reports zero
    ///    dedup activity.
    /// 3. `record_local_causality_event` poison → causality
    ///    clock stops advancing.
    /// 4. `causality_snapshot` poison → snapshot reports default
    ///    (zero) clock state.
    /// 5. `record_timestamp` poison → lag-timing data stops
    ///    collecting; `oldest_lag_ms` will report stale or
    ///    None going forward.
    /// 6. `oldest_lag_ms` poison → reports None for that channel
    ///    even when events are queued.
    ///
    /// `#![forbid(unsafe_code)]` and disciplined error handling
    /// make panics rare in this codebase, so this counter should
    /// stay at zero in healthy operation. Any non-zero value
    /// signals that the bus has lost observability for the
    /// current session and operators should investigate the
    /// originating panic in the tracing log.
    ///
    /// Same observability defect family as ft-luav8 (audit-failure
    /// counter), ft-0texd (policy clock-anomaly counter),
    /// ft-8cyii (events_dropped_dedup), ft-8na0z (proxy mount-
    /// failure counter), ft-2fjx0 (audit deadline-overflow),
    /// ft-647cj (mcp_bridge degraded-mode), ft-153dy (proxy
    /// destructive-tool-filtered) — make silent-failure
    /// surfaces observable instead of implicit.
    pub bus_lock_poisoned_count: AtomicU64,
    /// br-ft-tpdl5: cumulative count of `is_duplicate_delta_event`
    /// calls observed when the cuckoo filter's load factor was at
    /// or above the saturation threshold (0.95).
    ///
    /// The cuckoo filter is fixed-capacity (DEFAULT_CAPACITY=2000).
    /// Past saturation, NEW keys silently fail to insert (their
    /// verdict is still `New` because lookup misses, but the key
    /// is never recorded — so the next observation of the same
    /// key is also `New` instead of `PossibleDuplicate`). Effective
    /// dedup is silently disabled for any key that first appears
    /// post-saturation.
    ///
    /// This counter increments once per `is_duplicate_delta_event`
    /// call observed at saturation so operators can:
    /// - Detect that the dedup gate is no longer working as
    ///   intended (counter > 0 means dedup is degraded).
    /// - Alert on the rate (a steady non-zero rate signals a
    ///   missing rotation/clear policy).
    /// - Distinguish "events_dropped_dedup is low because there
    ///   are no duplicates" from "events_dropped_dedup is low
    ///   because the filter stopped catching them".
    ///
    /// Same observability defect family as ft-luav8 / ft-skec1 /
    /// ft-8na0z — make silent state loss visible.
    pub delta_dedup_full_count: AtomicU64,
}

#[derive(Debug, Default)]
struct ChannelLagTracker {
    sent_seq: AtomicU64,
    subscriber_positions: Mutex<Vec<Weak<AtomicU64>>>,
}

impl ChannelLagTracker {
    fn register(&self) -> Arc<AtomicU64> {
        let position = Arc::new(AtomicU64::new(self.sent_seq.load(Ordering::Relaxed)));
        if let Ok(mut guard) = self.subscriber_positions.lock() {
            guard.push(Arc::downgrade(&position));
        }
        position
    }

    fn record_send(&self) {
        self.sent_seq.fetch_add(1, Ordering::Relaxed);
    }

    fn queued_len(&self) -> usize {
        let sent = self.sent_seq.load(Ordering::Relaxed);
        let mut positions = match self.subscriber_positions.lock() {
            Ok(guard) => guard,
            Err(_) => return 0,
        };
        positions.retain(|position| position.strong_count() > 0);
        let Some(slowest) = positions
            .iter()
            .filter_map(Weak::upgrade)
            .map(|position| position.load(Ordering::Relaxed))
            .min()
        else {
            return 0;
        };
        usize::try_from(sent.saturating_sub(slowest)).unwrap_or(usize::MAX)
    }
}

impl EventBusMetrics {
    /// Create new metrics instance
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Get snapshot of current metrics
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            events_published: self.events_published.load(Ordering::Relaxed),
            events_dropped_no_subscribers: self
                .events_dropped_no_subscribers
                .load(Ordering::Relaxed),
            // br-ft-8cyii: dedup-drop counter for forensic
            // verification. See struct field docstring.
            events_dropped_dedup: self.events_dropped_dedup.load(Ordering::Relaxed),
            // br-ft-2z16v: distinct-event delivery counter. Closes
            // the forensic invariant in event-units (not fanout).
            events_delivered: self.events_delivered.load(Ordering::Relaxed),
            active_subscribers: self.active_subscribers.load(Ordering::Relaxed),
            subscriber_lag_events: self.subscriber_lag_events.load(Ordering::Relaxed),
            // br-ft-skec1: surface mutex-poison silent-failure count
            // so operators can detect lost dedup/causality/lag-timing
            // observability from a single MetricsSnapshot read.
            bus_lock_poisoned_count: self.bus_lock_poisoned_count.load(Ordering::Relaxed),
            // br-ft-tpdl5: surface cuckoo-saturation count so
            // operators can detect silent dedup-disable past the
            // 2000-key fixed capacity.
            delta_dedup_full_count: self.delta_dedup_full_count.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of event bus metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Total events published
    pub events_published: u64,
    /// Events dropped due to no subscribers
    pub events_dropped_no_subscribers: u64,
    /// br-ft-8cyii: events dropped by the cuckoo-dedup gate at
    /// `EventBus::publish`. See `events_delivered` below for the
    /// closed forensic invariant in event-units.
    ///
    /// br-ft-e3wwx: this number CONFLATES true duplicates with
    /// cuckoo false positives (≤ 5% FPR ceiling). Read it as an
    /// upper bound on intended dedup work + lower bound on FPR-
    /// induced delivery loss. True-duplicates ≥ value × 0.95;
    /// FPR data-loss ≤ value × 0.05. See the runtime field's
    /// docstring for the full operator-interpretation contract.
    #[serde(default)]
    pub events_dropped_dedup: u64,
    /// br-ft-2z16v: count of distinct events that reached at least
    /// one subscriber. NOT the fanout — incremented exactly once
    /// per published event whose `delivered` tally was > 0.
    ///
    /// Forensic verification (event-units, holds for every
    /// subscriber topology):
    /// ```text
    /// events_published == events_delivered
    ///                      + events_dropped_no_subscribers
    ///                      + events_dropped_dedup
    /// ```
    #[serde(default)]
    pub events_delivered: u64,
    /// Current active subscriber count
    pub active_subscribers: u64,
    /// br-ft-lb5x7: snapshot of the runtime
    /// [`EventBusMetrics::subscriber_lag_events`] counter.
    ///
    /// **Fanout-weighted** — mixes three semantics:
    /// 1. `recv_cx` Lagged → `+missed_count` per subscriber.
    /// 2. `try_recv` Lagged → `+missed_count` per subscriber.
    /// 3. `send_routed` Err with active subscribers → `+1`.
    ///
    /// Read this as "subscriber-missed-message + send-failure
    /// event count", NOT "distinct events lagged". See the
    /// runtime field's docstring for the full semantics.
    pub subscriber_lag_events: u64,

    /// br-ft-skec1: snapshot of
    /// [`EventBusMetrics::bus_lock_poisoned_count`] —
    /// cumulative count of internal Mutex-poison fall-through
    /// events. Non-zero means dedup/causality/lag-timing have
    /// been silently disabled for the current process. See the
    /// runtime field's docstring for the six contributing
    /// sources.
    #[serde(default)]
    pub bus_lock_poisoned_count: u64,

    /// br-ft-tpdl5: snapshot of
    /// [`EventBusMetrics::delta_dedup_full_count`] —
    /// cumulative count of dedup-check calls observed at or above
    /// the cuckoo filter's saturation threshold (0.95).
    ///
    /// Non-zero values signal that the dedup gate is no longer
    /// catching duplicates of newly-seen keys. The cuckoo filter
    /// is fixed-capacity; past saturation, new keys silently fail
    /// to insert and are never recognized as duplicates on
    /// subsequent observations.
    #[serde(default)]
    pub delta_dedup_full_count: u64,
}

/// Snapshot of queue depth and lag metrics per channel
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventBusStats {
    /// Queue capacity for each channel
    pub capacity: usize,
    /// Buffered delta events
    pub delta_queued: usize,
    /// Buffered detection events
    pub detection_queued: usize,
    /// Buffered signal events
    pub signal_queued: usize,
    /// Delta channel subscribers
    pub delta_subscribers: usize,
    /// Detection channel subscribers
    pub detection_subscribers: usize,
    /// Signal channel subscribers
    pub signal_subscribers: usize,
    /// Age of oldest delta event (ms)
    pub delta_oldest_lag_ms: Option<u64>,
    /// Age of oldest detection event (ms)
    pub detection_oldest_lag_ms: Option<u64>,
    /// Age of oldest signal event (ms)
    pub signal_oldest_lag_ms: Option<u64>,
    /// Approximate high-volume delta-event dedup state for doctor/status output.
    pub delta_dedup: EventCuckooDedupSnapshot,
    /// Local causality-clock frontier for distributed-event readiness.
    pub causality: EventCausalitySnapshot,
}

/// Event bus for distributing events to subscribers via broadcast fanout
///
/// Uses the compat broadcast channel for multi-consumer delivery. The bus is
/// bounded to provide backpressure - if a subscriber falls behind, it
/// will receive a lag error and miss intermediate messages.
pub struct EventBus {
    /// Broadcast sender for all events
    all_sender: broadcast::Sender<Event>,
    /// Broadcast sender for delta events
    delta_sender: broadcast::Sender<Event>,
    /// Broadcast sender for detection events
    detection_sender: broadcast::Sender<Event>,
    /// Broadcast sender for signal events
    signal_sender: broadcast::Sender<Event>,
    /// Queue capacity
    capacity: usize,
    /// Shared metrics
    metrics: Arc<EventBusMetrics>,
    /// Creation time for uptime tracking
    created_at: Instant,
    /// Delta queue timestamps (for lag metrics)
    delta_times: Mutex<VecDeque<Instant>>,
    /// Detection queue timestamps (for lag metrics)
    detection_times: Mutex<VecDeque<Instant>>,
    /// Signal queue timestamps (for lag metrics)
    signal_times: Mutex<VecDeque<Instant>>,
    /// Delta subscriber lag tracker
    delta_tracker: ChannelLagTracker,
    /// Detection subscriber lag tracker
    detection_tracker: ChannelLagTracker,
    /// Signal subscriber lag tracker
    signal_tracker: ChannelLagTracker,
    /// Approximate dedup for high-volume non-safety delta events.
    delta_dedup: Mutex<EventCuckooDedup>,
    /// Local causality clock for event ordering and future cross-process merge.
    causality_clock: Mutex<EventCausalityClock>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1000)
    }
}

impl EventBus {
    /// Create a new event bus with specified queue capacity
    ///
    /// # Arguments
    /// * `capacity` - Maximum number of events that can be buffered before
    ///   slow subscribers start lagging
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let (all_sender, _) = broadcast::channel(capacity);
        let (delta_sender, _) = broadcast::channel(capacity);
        let (detection_sender, _) = broadcast::channel(capacity);
        let (signal_sender, _) = broadcast::channel(capacity);
        Self {
            all_sender,
            delta_sender,
            detection_sender,
            signal_sender,
            capacity,
            metrics: Arc::new(EventBusMetrics::new()),
            created_at: Instant::now(),
            delta_times: Mutex::new(VecDeque::with_capacity(capacity)),
            detection_times: Mutex::new(VecDeque::with_capacity(capacity)),
            signal_times: Mutex::new(VecDeque::with_capacity(capacity)),
            delta_tracker: ChannelLagTracker::default(),
            detection_tracker: ChannelLagTracker::default(),
            signal_tracker: ChannelLagTracker::default(),
            delta_dedup: Mutex::new(EventCuckooDedup::default()),
            causality_clock: Mutex::new(EventCausalityClock::default()),
        }
    }

    /// Get the queue capacity
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get the number of active subscribers
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        crate::runtime_async::broadcast_receiver_count(&self.all_sender)
            + crate::runtime_async::broadcast_receiver_count(&self.delta_sender)
            + crate::runtime_async::broadcast_receiver_count(&self.detection_sender)
            + crate::runtime_async::broadcast_receiver_count(&self.signal_sender)
    }

    /// Get shared reference to metrics
    #[must_use]
    pub fn metrics(&self) -> Arc<EventBusMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Get uptime since bus creation
    #[must_use]
    pub fn uptime(&self) -> std::time::Duration {
        self.created_at.elapsed()
    }

    /// Publish an event to all subscribers
    ///
    /// This is a non-blocking operation. If there are no subscribers,
    /// the event is dropped and counted in metrics. If subscribers exist,
    /// the event is broadcast to all of them.
    ///
    /// Returns the number of subscribers that received the event.
    #[must_use]
    pub fn publish(&self, event: Event) -> usize {
        let capacity_timer = crate::runtime_telemetry::SwarmCapacityStageTimer::start(
            crate::runtime_telemetry::SwarmCapacityStage::EventBusFanout,
            u64::try_from(self.subscriber_count()).unwrap_or(u64::MAX),
        );
        self.metrics
            .events_published
            .fetch_add(1, Ordering::Relaxed);
        if self.is_duplicate_delta_event(&event) {
            // br-ft-8cyii: bump the dedup-drop counter so the
            // forensic invariant holds:
            // events_published == delivered
            //                      + events_dropped_no_subscribers
            //                      + events_dropped_dedup
            self.metrics
                .events_dropped_dedup
                .fetch_add(1, Ordering::Relaxed);
            capacity_timer.finish_completion();
            return 0;
        }
        self.record_local_causality_event();

        let mut delivered = 0usize;

        if let Ok(count) = crate::runtime_async::broadcast_send(&self.all_sender, event.clone()) {
            delivered += count;
        }

        delivered += match event {
            Event::SegmentCaptured { .. } | Event::GapDetected { .. } => self.send_routed(
                event,
                &self.delta_sender,
                &self.delta_times,
                &self.delta_tracker,
            ),
            Event::PatternDetected { .. } => self.send_routed(
                event,
                &self.detection_sender,
                &self.detection_times,
                &self.detection_tracker,
            ),
            Event::PaneDiscovered { .. }
            | Event::PaneDisappeared { .. }
            | Event::WorkflowStarted { .. }
            | Event::WorkflowStep { .. }
            | Event::WorkflowCompleted { .. }
            | Event::UserVarReceived { .. } => self.send_routed(
                event,
                &self.signal_sender,
                &self.signal_times,
                &self.signal_tracker,
            ),
        };

        if delivered == 0 {
            self.metrics
                .events_dropped_no_subscribers
                .fetch_add(1, Ordering::Relaxed);
        } else {
            // br-ft-2z16v: bump events_delivered by exactly 1 per
            // published event with at least one subscriber, NOT by
            // the fanout. This restores the closed forensic
            // invariant in event-units:
            //   events_published == events_delivered
            //                       + events_dropped_no_subscribers
            //                       + events_dropped_dedup
            self.metrics
                .events_delivered
                .fetch_add(1, Ordering::Relaxed);
        }

        capacity_timer.finish_completion();
        delivered
    }

    /// Create a new subscriber to receive events
    ///
    /// The subscriber will receive all events published after subscription.
    /// Events published before subscription are not received.
    #[must_use]
    pub fn subscribe(&self) -> EventSubscriber {
        self.metrics
            .active_subscribers
            .fetch_add(1, Ordering::Relaxed);
        EventSubscriber {
            receiver: self.all_sender.subscribe(),
            metrics: Arc::clone(&self.metrics),
            lagged_count: 0,
            observed_seq: None,
        }
    }

    /// Subscribe to delta events (segments and gaps)
    #[must_use]
    pub fn subscribe_deltas(&self) -> EventSubscriber {
        self.metrics
            .active_subscribers
            .fetch_add(1, Ordering::Relaxed);
        EventSubscriber {
            receiver: self.delta_sender.subscribe(),
            metrics: Arc::clone(&self.metrics),
            lagged_count: 0,
            observed_seq: Some(self.delta_tracker.register()),
        }
    }

    /// Subscribe to detection events
    #[must_use]
    pub fn subscribe_detections(&self) -> EventSubscriber {
        self.metrics
            .active_subscribers
            .fetch_add(1, Ordering::Relaxed);
        EventSubscriber {
            receiver: self.detection_sender.subscribe(),
            metrics: Arc::clone(&self.metrics),
            lagged_count: 0,
            observed_seq: Some(self.detection_tracker.register()),
        }
    }

    /// Subscribe to signal events (pane/workflow lifecycle)
    #[must_use]
    pub fn subscribe_signals(&self) -> EventSubscriber {
        self.metrics
            .active_subscribers
            .fetch_add(1, Ordering::Relaxed);
        EventSubscriber {
            receiver: self.signal_sender.subscribe(),
            metrics: Arc::clone(&self.metrics),
            lagged_count: 0,
            observed_seq: Some(self.signal_tracker.register()),
        }
    }

    /// Snapshot queue depths and oldest message lag per channel
    #[must_use]
    pub fn stats(&self) -> EventBusStats {
        let delta_queued = self.delta_tracker.queued_len();
        let detection_queued = self.detection_tracker.queued_len();
        let signal_queued = self.signal_tracker.queued_len();

        EventBusStats {
            capacity: self.capacity,
            delta_queued,
            detection_queued,
            signal_queued,
            delta_subscribers: crate::runtime_async::broadcast_receiver_count(&self.delta_sender),
            detection_subscribers: crate::runtime_async::broadcast_receiver_count(
                &self.detection_sender,
            ),
            signal_subscribers: crate::runtime_async::broadcast_receiver_count(&self.signal_sender),
            delta_oldest_lag_ms: Self::oldest_lag_ms(
                &self.delta_times,
                delta_queued,
                &self.metrics.bus_lock_poisoned_count,
            ),
            detection_oldest_lag_ms: Self::oldest_lag_ms(
                &self.detection_times,
                detection_queued,
                &self.metrics.bus_lock_poisoned_count,
            ),
            signal_oldest_lag_ms: Self::oldest_lag_ms(
                &self.signal_times,
                signal_queued,
                &self.metrics.bus_lock_poisoned_count,
            ),
            delta_dedup: self.delta_dedup_snapshot(),
            causality: self.causality_snapshot(),
        }
    }

    /// Merge a remote causality stamp into the local event-bus frontier.
    ///
    /// This is currently a substrate for future distributed event delivery; the
    /// single-process bus uses [`EventBus::publish`] to advance local time.
    pub fn observe_remote_causality(
        &self,
        remote: &EventCausalityStamp,
    ) -> Option<EventCausalityStamp> {
        self.causality_clock
            .lock()
            .ok()
            .map(|mut clock| clock.observe_remote(remote, current_unix_time_ms()))
    }

    /// br-ft-skec1: bump the bus_lock_poisoned_count counter.
    /// Called at every site where a Mutex::lock() returns Err
    /// and the runtime falls through with a default value
    /// (silent-degradation gate). Non-zero values surface lost
    /// dedup/causality/lag-timing observability.
    fn record_bus_lock_poisoned(&self) {
        self.metrics
            .bus_lock_poisoned_count
            .fetch_add(1, Ordering::Relaxed);
    }

    /// br-ft-tpdl5: cuckoo-filter saturation threshold. At or above
    /// this load_factor, NEW keys silently fail to insert into the
    /// fixed-capacity filter — their first observation is `New` (no
    /// dedup) but the key isn't recorded, so subsequent observations
    /// are ALSO `New` (still no dedup). Effective dedup is disabled
    /// for any key first seen post-saturation.
    ///
    /// 0.95 mirrors the threshold suggested in the EventCuckooDedup
    /// docstring (events_dedup_cuckoo.rs:139). Sub-saturation observations
    /// don't bump the counter; the metric reads "how often is the
    /// gate operating in degraded mode?".
    const DELTA_DEDUP_SATURATION_THRESHOLD: f64 = 0.95;

    fn is_duplicate_delta_event(&self, event: &Event) -> bool {
        let Some(key) = Self::delta_dedup_key(event) else {
            return false;
        };
        match self.delta_dedup.lock() {
            Ok(mut dedup) => {
                // br-ft-tpdl5: surface saturation BEFORE the check so
                // the counter reflects the state the check observes.
                // Pre-fix the cuckoo filter silently discarded inserts
                // past the 2000-key default capacity — operators had
                // no way to detect that the dedup gate had stopped
                // catching duplicates of newly-seen keys.
                if dedup.load_factor() >= Self::DELTA_DEDUP_SATURATION_THRESHOLD {
                    self.metrics
                        .delta_dedup_full_count
                        .fetch_add(1, Ordering::Relaxed);
                }
                dedup.check(&key) == CuckooDedupVerdict::PossibleDuplicate
            }
            Err(_) => {
                // br-ft-skec1 site #1: dedup mutex poisoned →
                // duplicates leak through. Surface the loss.
                self.record_bus_lock_poisoned();
                false
            }
        }
    }

    fn delta_dedup_snapshot(&self) -> EventCuckooDedupSnapshot {
        match self.delta_dedup.lock() {
            Ok(dedup) => dedup.snapshot(),
            Err(_) => {
                // br-ft-skec1 site #2: dedup mutex poisoned →
                // snapshot reports empty default. Surface the loss.
                self.record_bus_lock_poisoned();
                EventCuckooDedup::default().snapshot()
            }
        }
    }

    fn record_local_causality_event(&self) {
        match self.causality_clock.lock() {
            Ok(mut clock) => {
                let _ = clock.record_local_event(current_unix_time_ms());
            }
            Err(_) => {
                // br-ft-skec1 site #3: causality clock mutex
                // poisoned → clock stops advancing. Surface the
                // loss.
                self.record_bus_lock_poisoned();
            }
        }
    }

    fn causality_snapshot(&self) -> EventCausalitySnapshot {
        match self.causality_clock.lock() {
            Ok(clock) => clock.snapshot(),
            Err(_) => {
                // br-ft-skec1 site #4: causality mutex poisoned
                // → snapshot reports zero clock state. Surface
                // the loss.
                self.record_bus_lock_poisoned();
                EventCausalityClock::default().snapshot()
            }
        }
    }

    fn delta_dedup_key(event: &Event) -> Option<String> {
        match event {
            Event::SegmentCaptured {
                pane_id,
                seq,
                content_len,
            } => Some(format!("segment:{pane_id}:{seq}:{content_len}")),
            Event::GapDetected {
                pane_id,
                seq_before,
                seq_after,
                reason,
                detected_at_ms,
            } => Some(format!(
                "gap:{pane_id}:{seq_before}:{seq_after}:{detected_at_ms}:{reason}"
            )),
            _ => None,
        }
    }

    fn send_routed(
        &self,
        event: Event,
        sender: &broadcast::Sender<Event>,
        times: &Mutex<VecDeque<Instant>>,
        tracker: &ChannelLagTracker,
    ) -> usize {
        // Bump sent_seq BEFORE broadcast_send so subscribers can't fetch_add
        // their position counter and outrun sent_seq (which would underflow
        // saturating_sub in queued_len and transiently report 0). If the send
        // fails with no subscribers, over-counting is harmless (positions is
        // empty → queued_len returns 0). If subscribers exist but lag, the
        // extra sent_seq correctly reflects that one event was produced but
        // not consumed.
        tracker.record_send();
        match crate::runtime_async::broadcast_send(sender, event) {
            Ok(count) => {
                Self::record_timestamp(times, self.capacity, &self.metrics.bus_lock_poisoned_count);
                count
            }
            Err(_) => {
                // br-ft-lb5x7: bump source #3 (send-side, +1 per
                // failed send when active subscribers exist).
                // asupersync's broadcast_send returns Err only on
                // zero receivers; the receiver-count check guards
                // against bumping on the genuine no-subscriber
                // case. Race-window: receivers can drop between
                // the count check and the bump, in which case
                // this fires for what is effectively a no-
                // subscriber send. Distinct semantic from the
                // recv-side Lagged bumps in Subscriber::recv_cx
                // and Subscriber::try_recv (both fetch_add(n) per
                // subscriber observing a Lagged error). See
                // EventBusMetrics::subscriber_lag_events docstring
                // for the three-source mixing contract.
                if crate::runtime_async::broadcast_receiver_count(sender) > 0 {
                    self.metrics
                        .subscriber_lag_events
                        .fetch_add(1, Ordering::Relaxed);
                }
                0
            }
        }
    }

    /// br-ft-skec1: takes `poison_counter: &AtomicU64` so the
    /// caller's `EventBusMetrics::bus_lock_poisoned_count` can
    /// be bumped on the silent-degradation path. Existing call
    /// sites (production: send_routed; tests: free-standing)
    /// pass either `&self.metrics.bus_lock_poisoned_count` or a
    /// fresh `AtomicU64::new(0)` respectively.
    fn record_timestamp(
        times: &Mutex<VecDeque<Instant>>,
        capacity: usize,
        poison_counter: &AtomicU64,
    ) {
        match times.lock() {
            Ok(mut guard) => {
                guard.push_back(Instant::now());
                if guard.len() > capacity {
                    guard.pop_front();
                }
            }
            Err(_) => {
                // br-ft-skec1 site #5: lag-timing mutex poisoned
                // → push silently dropped; oldest_lag_ms will
                // report stale or None going forward. Surface
                // the loss via the caller-supplied counter.
                poison_counter.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// br-ft-skec1: companion to `record_timestamp`; takes
    /// `poison_counter` for the same reason. Returning `None`
    /// from a poisoned lock is indistinguishable from "no events
    /// queued" without the counter, which is the bug.
    fn oldest_lag_ms(
        times: &Mutex<VecDeque<Instant>>,
        queued_len: usize,
        poison_counter: &AtomicU64,
    ) -> Option<u64> {
        if queued_len == 0 {
            return None;
        }

        let oldest = match times.lock() {
            Ok(guard) => {
                let idx = guard.len().saturating_sub(queued_len);
                guard.get(idx).copied()?
            }
            Err(_) => {
                // br-ft-skec1 site #6: lag-times mutex poisoned
                // → reports None for that channel even though
                // events are queued. Surface the loss.
                poison_counter.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        };
        let elapsed_ms = oldest.elapsed().as_millis();
        u64::try_from(elapsed_ms).ok()
    }
}

/// Error returned when receiving events
#[derive(Debug, Clone)]
pub enum RecvError {
    /// The event bus was closed (all senders dropped)
    Closed,
    /// The caller's capability context was cancelled.
    Cancelled,
    /// Subscriber fell behind and missed events
    Lagged { missed_count: u64 },
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "event bus closed"),
            Self::Cancelled => write!(f, "event subscriber cancelled"),
            Self::Lagged { missed_count } => {
                write!(f, "subscriber lagged, missed {missed_count} events")
            }
        }
    }
}

impl std::error::Error for RecvError {}

const EVENT_SUBSCRIBER_CANCEL_POLL: Duration = Duration::from_millis(50);

/// Subscriber handle for receiving events from the bus
///
/// Dropping the subscriber automatically unsubscribes and decrements metrics.
pub struct EventSubscriber {
    receiver: broadcast::Receiver<Event>,
    metrics: Arc<EventBusMetrics>,
    lagged_count: u64,
    observed_seq: Option<Arc<AtomicU64>>,
}

impl EventSubscriber {
    /// Receive the next event
    ///
    /// Blocks until an event is available or the bus is closed.
    ///
    /// # Errors
    /// - `RecvError::Closed` if the event bus was dropped
    /// - `RecvError::Cancelled` if `cx` is cancelled while waiting
    /// - `RecvError::Lagged` if this subscriber fell behind (events were missed)
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.recv_cx(&cx).await
    }

    /// Receive the next event under an explicit `&Cx` (ft-xbnl0.2.2 Cx-first
    /// API).
    ///
    /// Cancellation, budget, and virtual time all flow through the provided
    /// capability context, so a canceled caller abandons the broadcast wait
    /// cleanly instead of blocking on thread-local state.
    ///
    /// # Errors
    /// - `RecvError::Closed` if the event bus was dropped
    /// - `RecvError::Cancelled` if `cx` is cancelled while waiting
    /// - `RecvError::Lagged` if this subscriber fell behind (events were missed)
    pub async fn recv_cx(&mut self, cx: &crate::cx::Cx) -> Result<Event, RecvError> {
        if cx.is_cancel_requested() {
            return Err(RecvError::Cancelled);
        }

        let recv_fut = std::pin::pin!(crate::runtime_async::broadcast_recv_with_cx(
            cx,
            &mut self.receiver
        ));
        let cancel_watcher = std::pin::pin!(async {
            loop {
                let _ = crate::runtime_async::sleep_with_cx(cx, EVENT_SUBSCRIBER_CANCEL_POLL).await;
                if cx.is_cancel_requested() {
                    return Err::<Event, RecvError>(RecvError::Cancelled);
                }
            }
        });

        match select(recv_fut, cancel_watcher).await {
            Either::Left((result, _)) => match result {
                Ok(event) => {
                    if let Some(position) = &self.observed_seq {
                        position.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(event)
                }
                Err(broadcast::RecvError::Closed) => Err(RecvError::Closed),
                Err(broadcast::RecvError::Cancelled) => Err(RecvError::Cancelled),
                Err(broadcast::RecvError::Lagged(n)) => {
                    self.lagged_count += n;
                    if let Some(position) = &self.observed_seq {
                        position.fetch_add(n, Ordering::Relaxed);
                    }
                    // br-ft-lb5x7: bump source #1 (recv-side,
                    // +missed_count per Lagged event PER SUBSCRIBER).
                    // With N subscribers all lagging on the same M
                    // events, the cumulative contribution from this
                    // site alone is N × M (fanout-weighted).
                    self.metrics
                        .subscriber_lag_events
                        .fetch_add(n, Ordering::Relaxed);
                    Err(RecvError::Lagged { missed_count: n })
                }
            },
            Either::Right((result, _)) => result,
        }
    }

    /// Try to receive an event without blocking
    ///
    /// Returns `None` if no event is immediately available.
    pub fn try_recv(&mut self) -> Option<Result<Event, RecvError>> {
        match crate::runtime_async::broadcast_try_recv(&mut self.receiver) {
            Ok(event) => {
                if let Some(position) = &self.observed_seq {
                    position.fetch_add(1, Ordering::Relaxed);
                }
                Some(Ok(event))
            }
            Err(crate::runtime_async::BroadcastTryRecvError::Empty) => None,
            Err(crate::runtime_async::BroadcastTryRecvError::Closed) => {
                Some(Err(RecvError::Closed))
            }
            Err(crate::runtime_async::BroadcastTryRecvError::Lagged(n)) => {
                self.lagged_count += n;
                if let Some(position) = &self.observed_seq {
                    position.fetch_add(n, Ordering::Relaxed);
                }
                // br-ft-lb5x7: bump source #2 (recv-side, same
                // shape as #1 but on the try_recv path). Same
                // fanout-weighted semantic — N subscribers
                // calling try_recv against M lagged events
                // contribute N × M.
                self.metrics
                    .subscriber_lag_events
                    .fetch_add(n, Ordering::Relaxed);
                Some(Err(RecvError::Lagged { missed_count: n }))
            }
        }
    }

    /// Get the total number of events this subscriber has missed due to lag
    #[must_use]
    pub fn lagged_count(&self) -> u64 {
        self.lagged_count
    }
}

impl Drop for EventSubscriber {
    fn drop(&mut self) {
        self.metrics
            .active_subscribers
            .fetch_sub(1, Ordering::Relaxed);
    }
}

// ---- Event deduplication with occurrence counting ----

/// Tracks per-key dedup state: occurrence count, first and last seen.
#[derive(Debug, Clone)]
pub struct DedupeEntry {
    /// Total occurrences of this event key
    pub count: u64,
    /// When the first occurrence was seen
    pub first_seen: Instant,
    /// When the most recent occurrence was seen
    pub last_seen: Instant,
}

/// Result of checking an event against the deduplicator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DedupeVerdict {
    /// First occurrence of this event key (or re-emerged after expiry).
    New,
    /// Duplicate within the dedup window. `suppressed_count` is how many
    /// duplicates have been suppressed since the first/re-emerged occurrence.
    Duplicate { suppressed_count: u64 },
}

/// Event deduplicator with occurrence counting and bounded capacity.
///
/// Collapses repeated identical events within a configurable time window.
/// Unlike `DetectionContext::mark_seen()`, this tracks how many duplicates
/// were suppressed and exposes first/last seen timestamps.
#[derive(Debug, Clone)]
pub struct EventDeduplicator {
    entries: HashMap<String, DedupeEntry>,
    insertion_order: VecDeque<String>,
    window: Duration,
    max_capacity: usize,
}

impl EventDeduplicator {
    /// Default dedup window: 5 minutes
    pub const DEFAULT_WINDOW: Duration = Duration::from_secs(5 * 60);
    /// Default maximum tracked keys
    pub const DEFAULT_MAX_CAPACITY: usize = 2000;

    /// Create a deduplicator with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            window: Self::DEFAULT_WINDOW,
            max_capacity: Self::DEFAULT_MAX_CAPACITY,
        }
    }

    /// Create a deduplicator with a custom window and capacity.
    #[must_use]
    pub fn with_config(window: Duration, max_capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            window,
            max_capacity,
        }
    }

    /// Check and record an event. Returns whether it's new or a duplicate.
    pub fn check(&mut self, key: &str) -> DedupeVerdict {
        // [ft-61kg4] Zero capacity means dedup is disabled; short-circuit
        // before any entry-map bookkeeping. Without this, the novel-key
        // eviction path at "if entries.len() >= max_capacity" is entered
        // with len=0 and cap=0, pop_front on empty insertion_order is a
        // no-op, and the insert below still succeeds — leaving a
        // 1-slot ghost cache where alternating keys toggle "New" while
        // immediate repeats spuriously return "Duplicate". Mirrors the
        // ft-bx4le fix in connector_inbound_bridge::SignalDeduplicator.
        if self.max_capacity == 0 {
            return DedupeVerdict::New;
        }
        let now = Instant::now();

        if let Some(entry) = self.entries.get_mut(key) {
            if now.duration_since(entry.last_seen) < self.window {
                // Within window: duplicate
                entry.count += 1;
                entry.last_seen = now;
                return DedupeVerdict::Duplicate {
                    suppressed_count: entry.count - 1,
                };
            }
            // Window expired: reset as new occurrence.
            // Refresh the insertion_order position so the next capacity
            // eviction pass treats this key as freshly inserted, not as
            // the same stale slot from its original insertion. Without
            // this refresh, a reset entry retains its old insertion_order
            // position and can be evicted by the next N novel-key checks
            // (where N is its distance from the front), which would then
            // cause a follow-up check of the same key within its reset
            // window to spuriously return `New` instead of `Duplicate`.
            entry.count = 1;
            entry.first_seen = now;
            entry.last_seen = now;
            if let Some(pos) = self.insertion_order.iter().position(|k| k == key) {
                self.insertion_order.remove(pos);
            }
            self.insertion_order.push_back(key.to_string());
            return DedupeVerdict::New;
        }

        // Never seen: evict oldest if at capacity
        if self.entries.len() >= self.max_capacity {
            if let Some(oldest_key) = self.insertion_order.pop_front() {
                self.entries.remove(&oldest_key);
            }
        }

        self.entries.insert(
            key.to_string(),
            DedupeEntry {
                count: 1,
                first_seen: now,
                last_seen: now,
            },
        );
        self.insertion_order.push_back(key.to_string());
        DedupeVerdict::New
    }

    /// Get the current entry for a key, if tracked and within the window.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&DedupeEntry> {
        let entry = self.entries.get(key)?;
        if Instant::now().duration_since(entry.last_seen) < self.window {
            Some(entry)
        } else {
            None
        }
    }

    /// Get the suppressed count for a key (0 if not tracked or expired).
    #[must_use]
    pub fn suppressed_count(&self, key: &str) -> u64 {
        self.get(key).map_or(0, |e| e.count.saturating_sub(1))
    }

    /// Number of tracked keys (including expired ones not yet evicted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the deduplicator has no tracked keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove all tracked entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
    }
}

impl Default for EventDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Notification cooldown ----

/// Tracks per-key notification cooldown state.
#[derive(Debug, Clone)]
pub struct CooldownEntry {
    /// When the last notification was sent
    pub last_notified: Instant,
    /// Events suppressed since the last notification
    pub suppressed_since_notify: u64,
}

/// Result of checking whether a notification should be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CooldownVerdict {
    /// Send the notification. `suppressed_since_last` is how many were
    /// suppressed since the previous notification (0 for the first).
    Send { suppressed_since_last: u64 },
    /// Suppress this notification (cooldown still active).
    Suppress { total_suppressed: u64 },
}

/// Notification cooldown tracker.
///
/// Prevents repeated notifications for the same event key within a
/// configurable cooldown period. When the cooldown expires, the next
/// occurrence sends a notification that includes the suppressed count.
#[derive(Debug, Clone)]
pub struct NotificationCooldown {
    entries: HashMap<String, CooldownEntry>,
    insertion_order: VecDeque<String>,
    cooldown: Duration,
    max_capacity: usize,
}

impl NotificationCooldown {
    /// Default cooldown: 30 seconds
    pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);
    /// Default maximum tracked keys
    pub const DEFAULT_MAX_CAPACITY: usize = 2000;

    /// Create a cooldown tracker with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            cooldown: Self::DEFAULT_COOLDOWN,
            max_capacity: Self::DEFAULT_MAX_CAPACITY,
        }
    }

    /// Create a cooldown tracker with a custom period and capacity.
    #[must_use]
    pub fn with_config(cooldown: Duration, max_capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            insertion_order: VecDeque::new(),
            cooldown,
            max_capacity,
        }
    }

    /// Check whether a notification should be sent for this key.
    ///
    /// On `Send`: the caller should send the notification and include
    /// `suppressed_since_last` in the message so operators know how many
    /// were collapsed.
    ///
    /// On `Suppress`: the caller should skip the notification.
    pub fn check(&mut self, key: &str) -> CooldownVerdict {
        // [ft-w80kj] Zero capacity means cooldown tracking is disabled; do
        // not retain hidden state for a single key. Without this guard, the
        // len>=max_capacity eviction branch is entered with len=0 and cap=0,
        // pop_front on empty insertion_order is a no-op, and the insert below
        // still succeeds — leaving a 1-slot ghost cooldown cache where one key
        // is silently throttled even though callers configured capacity=0.
        if self.max_capacity == 0 {
            return CooldownVerdict::Send {
                suppressed_since_last: 0,
            };
        }
        let now = Instant::now();

        if let Some(entry) = self.entries.get_mut(key) {
            if now.duration_since(entry.last_notified) < self.cooldown {
                // Still in cooldown: suppress
                entry.suppressed_since_notify += 1;
                return CooldownVerdict::Suppress {
                    total_suppressed: entry.suppressed_since_notify,
                };
            }
            // Cooldown expired: send with suppressed count
            let suppressed = entry.suppressed_since_notify;
            entry.last_notified = now;
            entry.suppressed_since_notify = 0;
            // [ft-hyrav] Refresh the insertion_order position so this
            // just-used key is the MOST recent, not stuck where it was
            // first inserted. Without this, a busy key that emits Send
            // repeatedly can be evicted by the next-novel-key path at
            // line 1047-1050 while a genuinely-dormant key with a
            // later insertion stays alive. Mirrors the same fix in
            // EventDeduplicator at events.rs:867-870 for window-expired
            // reset. The remove(pos)+push_back pair is O(n) but n <=
            // max_capacity so the cost is bounded by the caller's
            // chosen capacity.
            if let Some(pos) = self.insertion_order.iter().position(|k| k == key) {
                self.insertion_order.remove(pos);
            }
            self.insertion_order.push_back(key.to_string());
            return CooldownVerdict::Send {
                suppressed_since_last: suppressed,
            };
        }

        // First occurrence: evict oldest if at capacity
        if self.entries.len() >= self.max_capacity {
            if let Some(oldest_key) = self.insertion_order.pop_front() {
                self.entries.remove(&oldest_key);
            }
        }

        self.entries.insert(
            key.to_string(),
            CooldownEntry {
                last_notified: now,
                suppressed_since_notify: 0,
            },
        );
        self.insertion_order.push_back(key.to_string());
        CooldownVerdict::Send {
            suppressed_since_last: 0,
        }
    }

    /// Get the current cooldown entry for a key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&CooldownEntry> {
        self.entries.get(key)
    }

    /// Number of tracked keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cooldown tracker has no tracked keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove all tracked entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
    }
}

impl Default for NotificationCooldown {
    fn default() -> Self {
        Self::new()
    }
}

// ---- Event filter for notification gating ----

/// Converts a [`Severity`] to a numeric level for threshold comparisons.
///
/// Higher values indicate more severe events:
/// - Info = 0, Warning = 1, Critical = 2
fn severity_level(s: crate::patterns::Severity) -> u8 {
    match s {
        crate::patterns::Severity::Info => 0,
        crate::patterns::Severity::Warning => 1,
        crate::patterns::Severity::Critical => 2,
    }
}

/// Parse a severity string (case-insensitive) into a [`Severity`].
///
/// Accepts: "info", "warning", "critical" (and case variants).
/// Returns `None` for unrecognised strings.
fn parse_severity(s: &str) -> Option<crate::patterns::Severity> {
    match s.to_lowercase().as_str() {
        "info" => Some(crate::patterns::Severity::Info),
        "warning" => Some(crate::patterns::Severity::Warning),
        "critical" => Some(crate::patterns::Severity::Critical),
        _ => None,
    }
}

/// Parse an agent-type string (case-insensitive) into an [`AgentType`].
///
/// Accepts the serde-canonical names: "codex", "claude_code", "gemini",
/// "wezterm", "unknown".
fn parse_agent_type(s: &str) -> Option<crate::patterns::AgentType> {
    match s.to_lowercase().as_str() {
        "codex" => Some(crate::patterns::AgentType::Codex),
        "claude_code" => Some(crate::patterns::AgentType::ClaudeCode),
        "gemini" => Some(crate::patterns::AgentType::Gemini),
        "wezterm" => Some(crate::patterns::AgentType::Wezterm),
        "unknown" => Some(crate::patterns::AgentType::Unknown),
        _ => None,
    }
}

/// Simple glob matcher for rule-ID patterns.
///
/// Supports `*` (any sequence) and `?` (any single char).
/// Without wildcards, performs exact equality.
pub fn match_rule_glob(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return value == pattern;
    }

    let mut p_rem = pattern;
    let mut v_rem = value;

    let mut p_star = None;
    let mut v_star = None;

    while !v_rem.is_empty() {
        let mut p_chars = p_rem.chars();
        let mut v_chars = v_rem.chars();
        let p_ch = p_chars.next();
        let v_ch = v_chars.next().unwrap();

        if let Some(pc) = p_ch {
            if pc == '?' || pc == v_ch {
                p_rem = p_chars.as_str();
                v_rem = v_chars.as_str();
                continue;
            } else if pc == '*' {
                p_star = Some(p_chars.as_str());
                v_star = Some(v_rem);
                p_rem = p_chars.as_str();
                continue;
            }
        }

        if let Some(ps) = p_star {
            if let Some(vs) = v_star {
                p_rem = ps;
                let mut v_star_chars = vs.chars();
                v_star_chars.next();
                v_star = Some(v_star_chars.as_str());
                v_rem = v_star_chars.as_str();
                continue;
            }
        }

        return false;
    }

    while p_rem.starts_with('*') {
        p_rem = &p_rem[1..];
    }

    p_rem.is_empty()
}

/// Event notification filter.
///
/// Decides whether a [`Detection`] should trigger a notification based on
/// configurable include/exclude glob patterns, minimum severity, and
/// agent-type allowlist.
///
/// **Evaluation order:**
/// 1. Exclude patterns are checked first — if *any* match, the event is
///    filtered out (regardless of include rules).
/// 2. If `include` is non-empty, the rule-ID must match at least one
///    include pattern.
/// 3. Severity must meet or exceed `min_severity` (if set).
/// 4. Agent type must be in `agent_types` (if the list is non-empty).
#[derive(Debug, Clone)]
pub struct EventFilter {
    include: Vec<String>,
    exclude: Vec<String>,
    min_severity: Option<crate::patterns::Severity>,
    agent_types: Vec<crate::patterns::AgentType>,
}

impl EventFilter {
    /// Build a filter from raw config values.
    ///
    /// Unknown severity / agent-type strings are silently ignored so that
    /// forward-compatible config files don't break older binaries.
    #[must_use]
    pub fn from_config(
        include: &[String],
        exclude: &[String],
        min_severity: Option<&str>,
        agent_types: &[String],
    ) -> Self {
        Self {
            include: include.to_vec(),
            exclude: exclude.to_vec(),
            min_severity: min_severity.and_then(parse_severity),
            agent_types: agent_types
                .iter()
                .filter_map(|s| parse_agent_type(s))
                .collect(),
        }
    }

    /// Create a permissive filter that passes everything through.
    #[must_use]
    pub fn allow_all() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            min_severity: None,
            agent_types: Vec::new(),
        }
    }

    /// Returns `true` if the detection passes the filter and should be
    /// forwarded to the notification pipeline.
    #[must_use]
    pub fn matches(&self, detection: &Detection) -> bool {
        let rule_id = &detection.rule_id;

        // 1. Exclude wins
        if self.exclude.iter().any(|pat| match_rule_glob(pat, rule_id)) {
            return false;
        }

        // 2. Include (if non-empty, at least one must match)
        if !self.include.is_empty() && !self.include.iter().any(|pat| match_rule_glob(pat, rule_id))
        {
            return false;
        }

        // 3. Minimum severity
        if let Some(min) = self.min_severity {
            if severity_level(detection.severity) < severity_level(min) {
                return false;
            }
        }

        // 4. Agent type allowlist
        if !self.agent_types.is_empty() && !self.agent_types.contains(&detection.agent_type) {
            return false;
        }

        true
    }

    /// Returns `true` when the filter has no restrictions (equivalent to
    /// [`EventFilter::allow_all`]).
    #[must_use]
    pub fn is_permissive(&self) -> bool {
        self.include.is_empty()
            && self.exclude.is_empty()
            && self.min_severity.is_none()
            && self.agent_types.is_empty()
    }
}

impl Default for EventFilter {
    fn default() -> Self {
        Self::allow_all()
    }
}

/// Composite notification gate that combines filtering, deduplication, and
/// cooldown into a single decision point.
///
/// Typical usage in the runtime persistence task:
///
/// ```text
/// if gate.should_notify(&detection, pane_id, None) == NotifyDecision::Send { … }
/// ```
#[derive(Debug)]
pub struct NotificationGate {
    filter: EventFilter,
    dedup: EventDeduplicator,
    cooldown: NotificationCooldown,
}

/// Decision produced by [`NotificationGate::should_notify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyDecision {
    /// The event should produce a notification.
    Send {
        /// Number of similar events suppressed since the last notification.
        suppressed_since_last: u64,
    },
    /// The event was filtered out by pattern/severity/agent-type rules.
    Filtered,
    /// The event was suppressed as a duplicate within the dedup window.
    Deduplicated { suppressed_count: u64 },
    /// The event was suppressed by notification cooldown.
    Throttled { total_suppressed: u64 },
}

impl NotificationGate {
    /// Create a gate with the given filter, dedup, and cooldown settings.
    #[must_use]
    pub fn new(
        filter: EventFilter,
        dedup: EventDeduplicator,
        cooldown: NotificationCooldown,
    ) -> Self {
        Self {
            filter,
            dedup,
            cooldown,
        }
    }

    /// Create a gate from notification config values.
    #[must_use]
    pub fn from_config(
        filter: EventFilter,
        dedup_window: Duration,
        cooldown_period: Duration,
    ) -> Self {
        Self {
            filter,
            dedup: EventDeduplicator::with_config(
                dedup_window,
                EventDeduplicator::DEFAULT_MAX_CAPACITY,
            ),
            cooldown: NotificationCooldown::with_config(
                cooldown_period,
                NotificationCooldown::DEFAULT_MAX_CAPACITY,
            ),
        }
    }

    /// Decide whether a detection should produce a notification.
    ///
    /// The dedup key is derived from the event identity key
    /// (`rule_id`, `event_type`, `pane_uuid`/`pane_id`, and redacted extracted fields),
    /// so the same detection from different panes is treated independently.
    pub fn should_notify(
        &mut self,
        detection: &Detection,
        pane_id: u64,
        pane_uuid: Option<&str>,
    ) -> NotifyDecision {
        // Step 1: apply filter
        if !self.filter.matches(detection) {
            return NotifyDecision::Filtered;
        }

        let identity_key = event_identity_key(detection, pane_id, pane_uuid);

        // Step 2: dedup
        match self.dedup.check(&identity_key) {
            DedupeVerdict::Duplicate { suppressed_count } => {
                return NotifyDecision::Deduplicated { suppressed_count };
            }
            DedupeVerdict::New => {}
        }

        // Step 3: cooldown
        match self.cooldown.check(&identity_key) {
            CooldownVerdict::Suppress { total_suppressed } => {
                NotifyDecision::Throttled { total_suppressed }
            }
            CooldownVerdict::Send {
                suppressed_since_last,
            } => NotifyDecision::Send {
                suppressed_since_last,
            },
        }
    }

    /// Access the inner filter (e.g., for status output).
    #[must_use]
    pub fn filter(&self) -> &EventFilter {
        &self.filter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LabRuntime-based determinism test (ft-xbnl0.2.2): prove the Cx-first
    /// `EventSubscriber::recv_cx` path runs under seed-locked virtual-time
    /// scheduling. We publish a single event into the bus, drain it through
    /// `recv_cx(&cx)`, and assert the round trip happens without consuming
    /// real time. If the broadcast recv ever re-acquires a tokio-shaped
    /// (real-time) assumption, this test either step-explodes or burns
    /// real seconds.
    #[test]
    fn event_subscriber_recv_cx_runs_under_labruntime() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const SEED: u64 = 0xE5E7_57B5_4C41_2000;
        let wall_start = std::time::Instant::now();
        let event_observed = Arc::new(AtomicBool::new(false));
        let event_observed_task = Arc::clone(&event_observed);

        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(SEED)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(50_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let cx = crate::cx::for_request();
                let bus = EventBus::new(8);
                let mut sub = bus.subscribe();
                let _ = bus.publish(Event::SegmentCaptured {
                    pane_id: 1,
                    seq: 42,
                    content_len: 100,
                });
                match sub.recv_cx(&cx).await {
                    Ok(Event::SegmentCaptured { pane_id, seq, .. }) => {
                        assert_eq!(pane_id, 1);
                        assert_eq!(seq, 42);
                        event_observed_task.store(true, Ordering::SeqCst);
                    }
                    Ok(other) => panic!("unexpected event variant: {other:?}"),
                    Err(err) => panic!("recv_cx returned error: {err:?}"),
                }
            })
            .expect("spawn events recv_cx task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert!(
            event_observed.load(Ordering::SeqCst),
            "recv_cx must deliver the published event under LabRuntime"
        );
        assert!(
            report.oracle_report.all_passed(),
            "LabRuntime oracles must all pass: {report:?}"
        );
        assert!(
            wall_start.elapsed() < std::time::Duration::from_secs(1),
            "Cx-first broadcast recv must not burn real time; elapsed {:?}",
            wall_start.elapsed()
        );
    }

    #[test]
    fn event_subscriber_recv_cx_surfaces_pre_cancel_immediately() {
        run_async_test(async {
            let cx = crate::cx::Cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("event subscriber pre-cancel test"),
            );
            let bus = EventBus::new(8);
            let mut sub = bus.subscribe();

            let result =
                crate::runtime_async::timeout(Duration::from_millis(10), sub.recv_cx(&cx)).await;

            assert!(
                matches!(result, Ok(Err(RecvError::Cancelled))),
                "pre-cancel must return Err(Cancelled) immediately; got: {result:?}"
            );
        });
    }

    #[test]
    fn event_subscriber_recv_cx_surfaces_mid_flight_cancel() {
        run_async_test(async {
            let cx = crate::cx::Cx::for_testing();
            let cancel_cx = cx.clone();
            let bus = EventBus::new(8);
            let mut sub = bus.subscribe();

            std::mem::drop(crate::runtime_async::task::spawn(async move {
                crate::runtime_async::sleep(Duration::from_millis(100)).await;
                cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("event subscriber mid-flight cancel test"),
                );
            }));

            let started = Instant::now();
            let result = sub.recv_cx(&cx).await;
            let elapsed = started.elapsed();

            assert!(
                matches!(result, Err(RecvError::Cancelled)),
                "mid-flight cancel must surface as Err(Cancelled); got: {result:?}"
            );
            assert!(
                elapsed < Duration::from_secs(1),
                "mid-flight cancel should wake recv_cx within one poll cycle; took {elapsed:?}"
            );
        });
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build compat runtime for test");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn event_serializes() {
        let event = Event::SegmentCaptured {
            pane_id: 1,
            seq: 42,
            content_len: 100,
        };

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("segment_captured"));
    }

    #[test]
    fn bus_can_be_created() {
        let bus = EventBus::new(100);
        assert_eq!(bus.capacity(), 100);
    }

    #[test]
    fn event_type_name_matches_serde() {
        let event = Event::GapDetected {
            pane_id: 1,
            seq_before: 4,
            seq_after: 5,
            reason: "test".to_string(),
            detected_at_ms: 1234,
        };
        assert_eq!(event.type_name(), "gap_detected");

        let event = Event::WorkflowStarted {
            workflow_id: "w1".to_string(),
            workflow_name: "test".to_string(),
            pane_id: 1,
        };
        assert_eq!(event.type_name(), "workflow_started");
    }

    #[test]
    fn event_pane_id_extraction() {
        let event = Event::SegmentCaptured {
            pane_id: 42,
            seq: 1,
            content_len: 100,
        };
        assert_eq!(event.pane_id(), Some(42));

        let event = Event::WorkflowStep {
            workflow_id: "w1".to_string(),
            step_name: "step1".to_string(),
            result: "ok".to_string(),
        };
        assert_eq!(event.pane_id(), None);
    }

    #[test]
    fn vector_clock_detects_happens_before_and_concurrency() {
        let mut alpha = VectorClock::new();
        alpha.increment("alpha");

        let mut beta = alpha.clone();
        beta.increment("beta");

        assert_eq!(alpha.relation_to(&beta), CausalRelation::Before);
        assert_eq!(beta.relation_to(&alpha), CausalRelation::After);

        let mut gamma = VectorClock::new();
        gamma.increment("gamma");

        assert_eq!(beta.relation_to(&gamma), CausalRelation::Concurrent);
        assert_eq!(beta.relation_to(&beta), CausalRelation::Equal);
    }

    #[test]
    fn event_causality_clock_merges_remote_frontier() {
        let mut alpha = EventCausalityClock::new("alpha");
        let alpha_stamp = alpha.record_local_event(1_000);

        let mut beta = EventCausalityClock::new("beta");
        let beta_stamp = beta.observe_remote(&alpha_stamp, 900);

        assert_eq!(beta_stamp.lamport.counter, 2);
        assert_eq!(beta_stamp.vector.get("alpha"), 1);
        assert_eq!(beta_stamp.vector.get("beta"), 1);
        assert_eq!(
            alpha_stamp.vector.relation_to(&beta_stamp.vector),
            CausalRelation::Before
        );
        assert_eq!(beta_stamp.hybrid.wall_time_ms, 1_000);
        assert_eq!(
            beta_stamp.hybrid.logical, 1,
            "receive must advance logical component when remote wall time dominates"
        );
    }

    #[test]
    fn event_bus_publish_advances_causality_snapshot() {
        let bus = EventBus::new(10);
        assert_eq!(bus.stats().causality.lamport_counter, 0);

        let _ = bus.publish(Event::PaneDisappeared { pane_id: 1 });

        let stats = bus.stats();
        assert_eq!(stats.causality.node_id, DEFAULT_EVENT_CAUSALITY_NODE_ID);
        assert_eq!(stats.causality.lamport_counter, 1);
        assert_eq!(stats.causality.vector_nodes, 1);
    }

    #[test]
    fn event_bus_remote_causality_observation_merges_without_publish() {
        let mut remote = EventCausalityClock::new("remote");
        let remote_stamp = remote.record_local_event(10);
        let bus = EventBus::new(10);

        let merged = bus
            .observe_remote_causality(&remote_stamp)
            .expect("causality lock available");

        assert_eq!(merged.lamport.counter, 2);
        assert_eq!(merged.vector.get("remote"), 1);
        assert_eq!(merged.vector.get(DEFAULT_EVENT_CAUSALITY_NODE_ID), 1);
        assert_eq!(bus.stats().causality.vector_nodes, 2);
    }

    #[test]
    fn publish_with_no_subscribers_counts_drops() {
        run_async_test(async {
            let bus = EventBus::new(10);

            let count = bus.publish(Event::PaneDisappeared { pane_id: 1 });
            assert_eq!(count, 0);

            let metrics = bus.metrics().snapshot();
            assert_eq!(metrics.events_published, 1);
            assert_eq!(metrics.events_dropped_no_subscribers, 1);
        });
    }

    #[test]
    fn subscriber_receives_published_events() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut sub = bus.subscribe();

            let _ = bus.publish(Event::PaneDiscovered {
                pane_id: 1,
                domain: "local".to_string(),
                title: "shell".to_string(),
            });

            let event = sub.recv().await.unwrap();
            assert!(matches!(event, Event::PaneDiscovered { pane_id: 1, .. }));
        });
    }

    #[test]
    fn multiple_subscribers_fanout() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut sub1 = bus.subscribe();
            let mut sub2 = bus.subscribe();

            assert_eq!(bus.subscriber_count(), 2);

            let _ = bus.publish(Event::PaneDisappeared { pane_id: 42 });

            let e1 = sub1.recv().await.unwrap();
            let e2 = sub2.recv().await.unwrap();

            assert!(matches!(e1, Event::PaneDisappeared { pane_id: 42 }));
            assert!(matches!(e2, Event::PaneDisappeared { pane_id: 42 }));
        });
    }

    #[test]
    fn delta_subscriber_only_sees_delta_events() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut delta_sub = bus.subscribe_deltas();

            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 5,
                seq: 1,
                content_len: 10,
            });

            let event = delta_sub.recv().await.unwrap();
            assert!(matches!(event, Event::SegmentCaptured { pane_id: 5, .. }));

            let _ = bus.publish(Event::PaneDiscovered {
                pane_id: 5,
                domain: "local".to_string(),
                title: "shell".to_string(),
            });

            assert!(delta_sub.try_recv().is_none());
        });
    }

    #[test]
    fn routed_subscriber_counts_delivery_without_all_subscribers() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut delta_sub = bus.subscribe_deltas();

            let count = bus.publish(Event::SegmentCaptured {
                pane_id: 5,
                seq: 1,
                content_len: 10,
            });

            assert_eq!(count, 1);
            let metrics = bus.metrics().snapshot();
            assert_eq!(metrics.events_published, 1);
            assert_eq!(metrics.events_dropped_no_subscribers, 0);

            let event = delta_sub.recv().await.unwrap();
            assert!(matches!(event, Event::SegmentCaptured { pane_id: 5, .. }));
        });
    }

    #[test]
    fn delta_event_bus_cuckoo_dedup_suppresses_duplicate_delta_events() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut all_sub = bus.subscribe();
            let mut delta_sub = bus.subscribe_deltas();
            let event = Event::SegmentCaptured {
                pane_id: 5,
                seq: 1,
                content_len: 10,
            };

            let first_delivered = bus.publish(event.clone());
            assert_eq!(first_delivered, 2);
            assert!(matches!(
                all_sub.recv().await.unwrap(),
                Event::SegmentCaptured { pane_id: 5, .. }
            ));
            assert!(matches!(
                delta_sub.recv().await.unwrap(),
                Event::SegmentCaptured { pane_id: 5, .. }
            ));

            let duplicate_delivered = bus.publish(event);
            assert_eq!(
                duplicate_delivered, 0,
                "duplicate high-volume delta events should be suppressed before fanout"
            );
            assert!(all_sub.try_recv().is_none());
            assert!(delta_sub.try_recv().is_none());

            let stats = bus.stats();
            assert!(stats.delta_dedup.count > 0);
            assert_eq!(
                stats.delta_dedup.expected_items,
                EventCuckooDedup::DEFAULT_CAPACITY as u64
            );
            assert!(stats.delta_dedup.memory_bytes > 0);
        });
    }

    #[test]
    fn delta_event_bus_cuckoo_false_positive_rate_stays_below_five_percent() {
        let mut dedup = EventCuckooDedup::with_capacity(2000);
        for seq in 0..500 {
            let event = Event::SegmentCaptured {
                pane_id: 7,
                seq,
                content_len: 80,
            };
            let key = EventBus::delta_dedup_key(&event).expect("delta event key");
            assert_eq!(dedup.check(&key), CuckooDedupVerdict::New);
        }

        let mut false_positives = 0_u32;
        for seq in 10_000..11_000 {
            let event = Event::SegmentCaptured {
                pane_id: 7,
                seq,
                content_len: 80,
            };
            let key = EventBus::delta_dedup_key(&event).expect("delta event key");
            if dedup.check(&key) == CuckooDedupVerdict::PossibleDuplicate {
                false_positives += 1;
            }
        }

        let fpr = f64::from(false_positives) / 1000.0;
        assert!(
            fpr < 0.05,
            "delta event cuckoo dedup false-positive rate must stay below 5%; got {fpr:.4}"
        );
    }

    #[test]
    fn detection_subscriber_receives_pattern_events() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut detection_sub = bus.subscribe_detections();

            let detection = Detection {
                rule_id: "codex.test".to_string(),
                agent_type: crate::patterns::AgentType::Codex,
                event_type: "test".to_string(),
                severity: crate::patterns::Severity::Info,
                confidence: 0.9,
                extracted: serde_json::json!({}),
                matched_text: "anchor".to_string(),
                span: (0, 0),
            };

            let _ = bus.publish(Event::PatternDetected {
                pane_id: 1,
                pane_uuid: None,
                detection,
                event_id: None,
            });

            let event = detection_sub.recv().await.unwrap();
            assert!(matches!(event, Event::PatternDetected { pane_id: 1, .. }));
        });
    }

    #[test]
    fn subscriber_drop_decrements_count() {
        run_async_test(async {
            let bus = EventBus::new(10);

            {
                let _sub1 = bus.subscribe();
                let _sub2 = bus.subscribe();
                assert_eq!(bus.subscriber_count(), 2);
            }

            // After subscribers are dropped
            assert_eq!(bus.subscriber_count(), 0);

            let metrics = bus.metrics().snapshot();
            assert_eq!(metrics.active_subscribers, 0);
        });
    }

    #[test]
    fn try_recv_returns_none_when_empty() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut sub = bus.subscribe();

            assert!(sub.try_recv().is_none());
        });
    }

    #[test]
    fn try_recv_returns_event_when_available() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let mut sub = bus.subscribe();

            let _ = bus.publish(Event::PaneDisappeared { pane_id: 1 });

            let result = sub.try_recv();
            assert!(result.is_some());
            assert!(matches!(
                result.unwrap().unwrap(),
                Event::PaneDisappeared { pane_id: 1 }
            ));
        });
    }

    #[test]
    fn backpressure_causes_lag() {
        run_async_test(async {
            // Small capacity to trigger lag
            let bus = EventBus::new(2);
            let mut sub = bus.subscribe();

            // Publish more events than capacity
            for i in 0..5 {
                let _ = bus.publish(Event::SegmentCaptured {
                    pane_id: 1,
                    seq: i,
                    content_len: 10,
                });
            }

            // First recv should report lag
            let result = sub.recv().await;
            match result {
                Err(RecvError::Lagged { missed_count }) => {
                    assert!(missed_count > 0);
                }
                Ok(_) => {
                    // Might get an event if timing works out, that's ok too
                }
                Err(RecvError::Cancelled) => panic!("unexpected cancel"),
                Err(RecvError::Closed) => panic!("unexpected close"),
            }

            // Lag should be tracked in metrics
            let metrics = bus.metrics().snapshot();
            assert!(metrics.subscriber_lag_events > 0 || sub.lagged_count() > 0);
        });
    }

    #[test]
    fn stats_report_queue_depths_and_lag() {
        let bus = EventBus::new(2);
        let _delta_sub = bus.subscribe_deltas();

        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 0,
            content_len: 1,
        });
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 1,
        });
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 2,
            content_len: 1,
        });

        let stats = bus.stats();
        assert_eq!(stats.capacity, 2);
        assert_eq!(stats.delta_subscribers, 1);
        assert_eq!(stats.delta_queued, 2);
        assert!(stats.delta_oldest_lag_ms.is_some());
    }

    #[test]
    fn stats_clear_oldest_lag_when_queue_is_drained() {
        run_async_test(async {
            let bus = EventBus::new(4);
            let mut delta_sub = bus.subscribe_deltas();

            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq: 0,
                content_len: 1,
            });
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq: 1,
                content_len: 1,
            });

            delta_sub.recv().await.unwrap();
            delta_sub.recv().await.unwrap();

            let stats = bus.stats();
            assert_eq!(stats.delta_queued, 0);
            assert_eq!(stats.delta_oldest_lag_ms, None);
        });
    }

    #[test]
    fn oldest_lag_uses_oldest_buffered_timestamp() {
        let now = Instant::now();
        let times = Mutex::new(VecDeque::from([
            now.checked_sub(Duration::from_millis(30)).unwrap(),
            now.checked_sub(Duration::from_millis(20)).unwrap(),
            now.checked_sub(Duration::from_millis(10)).unwrap(),
        ]));

        // br-ft-skec1: free-standing test passes a fresh
        // poison_counter; healthy locks never bump it.
        let poison_counter = AtomicU64::new(0);
        let lag_one = EventBus::oldest_lag_ms(&times, 1, &poison_counter).unwrap();
        let lag_two = EventBus::oldest_lag_ms(&times, 2, &poison_counter).unwrap();
        let lag_three = EventBus::oldest_lag_ms(&times, 3, &poison_counter).unwrap();

        assert!(lag_three >= lag_two);
        assert!(lag_two >= lag_one);
        assert_eq!(EventBus::oldest_lag_ms(&times, 0, &poison_counter), None);
        assert_eq!(
            poison_counter.load(Ordering::Relaxed),
            0,
            "br-ft-skec1: healthy lock must not bump poison_counter"
        );
    }

    /// br-ft-skec1: when the times Mutex is poisoned, oldest_lag_ms
    /// returns None AND bumps the supplied poison_counter so the
    /// silent-degradation path is observable. Mirrors the bead's
    /// "force-poison via std::panic::catch_unwind" recipe.
    #[test]
    fn oldest_lag_ms_bumps_poison_counter_on_poisoned_lock() {
        let times = Mutex::new(VecDeque::from([Instant::now()]));
        // Poison the mutex by panicking inside a held guard.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = times.lock().unwrap();
            panic!("br-ft-skec1 force-poison");
        }));
        assert!(
            times.is_poisoned(),
            "panic-while-held must poison the mutex"
        );

        let poison_counter = AtomicU64::new(0);
        // queued_len > 0 forces the function past the early-return.
        let result = EventBus::oldest_lag_ms(&times, 1, &poison_counter);
        assert_eq!(result, None, "poisoned lock must surface as None");
        assert_eq!(
            poison_counter.load(Ordering::Relaxed),
            1,
            "br-ft-skec1: poisoned lock must bump poison_counter by 1"
        );
    }

    /// br-ft-skec1: record_timestamp's silent-degradation path
    /// (lag_times Mutex poisoned) bumps the poison_counter.
    #[test]
    fn record_timestamp_bumps_poison_counter_on_poisoned_lock() {
        let times: Mutex<VecDeque<Instant>> = Mutex::new(VecDeque::new());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = times.lock().unwrap();
            panic!("br-ft-skec1 force-poison");
        }));
        assert!(times.is_poisoned());

        let poison_counter = AtomicU64::new(0);
        EventBus::record_timestamp(&times, 16, &poison_counter);
        assert_eq!(
            poison_counter.load(Ordering::Relaxed),
            1,
            "br-ft-skec1: record_timestamp must bump poison_counter on poisoned lock"
        );
    }

    #[test]
    fn metrics_snapshot_is_serializable() {
        let metrics = MetricsSnapshot {
            events_published: 100,
            events_dropped_no_subscribers: 5,
            // br-ft-8cyii sibling-cleanup: missing field added.
            events_dropped_dedup: 0,
            // br-ft-2z16v: missing field added.
            events_delivered: 95,
            active_subscribers: 3,
            subscriber_lag_events: 10,
            // br-ft-skec1: missing field added.
            bus_lock_poisoned_count: 0,
            // br-ft-tpdl5: cuckoo saturation counter.
            delta_dedup_full_count: 0,
        };

        let json = serde_json::to_string(&metrics).unwrap();
        assert!(json.contains("events_published"));
        assert!(json.contains("100"));
    }

    #[test]
    fn default_bus_has_1000_capacity() {
        let bus = EventBus::default();
        assert_eq!(bus.capacity(), 1000);
    }

    #[test]
    fn recv_error_display() {
        run_async_test(async {
            let err = RecvError::Closed;
            assert_eq!(format!("{err}"), "event bus closed");

            let err = RecvError::Cancelled;
            assert_eq!(format!("{err}"), "event subscriber cancelled");

            let err = RecvError::Lagged { missed_count: 42 };
            assert_eq!(format!("{err}"), "subscriber lagged, missed 42 events");
        });
    }

    #[test]
    fn uptime_increases() {
        run_async_test(async {
            let bus = EventBus::new(10);
            let t1 = bus.uptime();
            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;
            let t2 = bus.uptime();
            assert!(t2 > t1);
        });
    }

    // ========================================================================
    // User-var payload decoding tests (wa-4vx.4.10)
    // ========================================================================

    #[test]
    fn user_var_decode_valid_base64_json() {
        use base64::Engine;

        // Encode {"type":"command_start","cmd":"ls -la"}
        let json = r#"{"type":"command_start","cmd":"ls -la"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);

        let payload = UserVarPayload::decode(&encoded, false).unwrap();

        assert_eq!(payload.event_type, Some("command_start".to_string()));
        assert!(payload.event_data.is_some());
        let data = payload.event_data.unwrap();
        assert_eq!(data.get("cmd").and_then(|v| v.as_str()), Some("ls -la"));
    }

    #[test]
    fn user_var_decode_invalid_base64_strict() {
        // Not valid base64
        let invalid = "!!!not-base64!!!";
        let result = UserVarPayload::decode(invalid, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, UserVarError::ParseFailed(_)));
        assert!(err.to_string().contains("invalid base64"));
    }

    #[test]
    fn user_var_decode_invalid_base64_lenient() {
        // Not valid base64, but lenient mode should return raw value
        let invalid = "!!!not-base64!!!";
        let payload = UserVarPayload::decode(invalid, true).unwrap();

        assert_eq!(payload.value, invalid);
        assert!(payload.event_type.is_none());
        assert!(payload.event_data.is_none());
    }

    #[test]
    fn user_var_decode_valid_base64_invalid_json_strict() {
        use base64::Engine;

        // Valid base64, but not valid JSON
        let not_json = "this is not json";
        let encoded = base64::engine::general_purpose::STANDARD.encode(not_json);

        let result = UserVarPayload::decode(&encoded, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, UserVarError::ParseFailed(_)));
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[test]
    fn user_var_decode_valid_base64_invalid_json_lenient() {
        use base64::Engine;

        // Valid base64, but not valid JSON - lenient mode returns raw value
        let not_json = "this is not json";
        let encoded = base64::engine::general_purpose::STANDARD.encode(not_json);

        let payload = UserVarPayload::decode(&encoded, true).unwrap();

        assert_eq!(payload.value, encoded);
        assert!(payload.event_type.is_none());
        assert!(payload.event_data.is_none());
    }

    #[test]
    fn user_var_decode_unknown_event_type() {
        use base64::Engine;

        // Unknown event type - should decode fine but event_type comes through
        let json = r#"{"type":"completely_unknown_event","data":"whatever"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);

        let payload = UserVarPayload::decode(&encoded, false).unwrap();

        // Should not panic, should capture the type
        assert_eq!(
            payload.event_type,
            Some("completely_unknown_event".to_string())
        );
        assert!(payload.event_data.is_some());
    }

    #[test]
    fn user_var_decode_missing_type_field() {
        use base64::Engine;

        // Valid JSON but missing "type" field
        let json = r#"{"data":"some data","other":"field"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);

        let payload = UserVarPayload::decode(&encoded, false).unwrap();

        // Should decode fine, just no event_type
        assert!(payload.event_type.is_none());
        assert!(payload.event_data.is_some());
    }

    #[test]
    fn user_var_decode_empty_json_object() {
        use base64::Engine;

        let json = "{}";
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);

        let payload = UserVarPayload::decode(&encoded, false).unwrap();

        assert!(payload.event_type.is_none());
        assert!(payload.event_data.is_some());
    }

    #[test]
    fn user_var_decode_invalid_utf8_strict() {
        use base64::Engine;

        // Valid base64 but contains invalid UTF-8 bytes
        let invalid_utf8: &[u8] = &[0xff, 0xfe, 0x00, 0x01];
        let encoded = base64::engine::general_purpose::STANDARD.encode(invalid_utf8);

        let result = UserVarPayload::decode(&encoded, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, UserVarError::ParseFailed(_)));
        assert!(err.to_string().contains("invalid UTF-8"));
    }

    #[test]
    fn user_var_decode_invalid_utf8_lenient() {
        use base64::Engine;

        // Valid base64 but contains invalid UTF-8 bytes - lenient mode
        let invalid_utf8: &[u8] = &[0xff, 0xfe, 0x00, 0x01];
        let encoded = base64::engine::general_purpose::STANDARD.encode(invalid_utf8);

        let payload = UserVarPayload::decode(&encoded, true).unwrap();

        // Should not panic, retains raw value
        assert_eq!(payload.value, encoded);
        assert!(payload.event_type.is_none());
        assert!(payload.event_data.is_none());
    }

    #[test]
    fn user_var_error_messages_are_actionable() {
        // Test error message clarity

        let err = UserVarError::WatcherNotRunning {
            socket_path: "/tmp/test.sock".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("not running"));
        assert!(msg.contains("/tmp/test.sock"));

        let err = UserVarError::IpcSendFailed {
            message: "connection refused".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("IPC"));
        assert!(msg.contains("connection refused"));

        let err = UserVarError::ParseFailed("invalid base64".to_string());
        let msg = err.to_string();
        assert!(msg.contains("parse"));
        assert!(msg.contains("invalid base64"));
    }

    #[test]
    fn user_var_payload_preserves_raw_value() {
        use base64::Engine;

        let json = r#"{"type":"test"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);

        let payload = UserVarPayload::decode(&encoded, false).unwrap();

        // Raw value should be preserved
        assert_eq!(payload.value, encoded);
    }

    #[test]
    fn user_var_received_event_routing() {
        // UserVarReceived should be routed to signal channel
        let bus = EventBus::new(10);
        let mut signal_sub = bus.subscribe_signals();
        let mut delta_sub = bus.subscribe_deltas();

        let payload = UserVarPayload {
            value: "test".to_string(),
            event_type: Some("test".to_string()),
            event_data: None,
        };

        let _ = bus.publish(Event::UserVarReceived {
            pane_id: 1,
            name: "FT_EVENT".to_string(),
            payload,
        });

        // Should be in signal channel
        assert!(signal_sub.try_recv().is_some());
        // Should NOT be in delta channel
        assert!(delta_sub.try_recv().is_none());
    }

    // ---- EventDeduplicator tests ----

    #[test]
    fn dedup_first_occurrence_is_new() {
        let mut dedup = EventDeduplicator::new();
        assert_eq!(dedup.check("key-a"), DedupeVerdict::New);
    }

    #[test]
    fn dedup_second_occurrence_is_duplicate() {
        let mut dedup = EventDeduplicator::new();
        assert_eq!(dedup.check("key-a"), DedupeVerdict::New);
        assert_eq!(
            dedup.check("key-a"),
            DedupeVerdict::Duplicate {
                suppressed_count: 1
            }
        );
    }

    #[test]
    fn dedup_counter_increments() {
        let mut dedup = EventDeduplicator::new();
        dedup.check("k");
        dedup.check("k");
        dedup.check("k");
        assert_eq!(
            dedup.check("k"),
            DedupeVerdict::Duplicate {
                suppressed_count: 3
            }
        );
    }

    #[test]
    fn dedup_different_keys_independent() {
        let mut dedup = EventDeduplicator::new();
        assert_eq!(dedup.check("a"), DedupeVerdict::New);
        assert_eq!(dedup.check("b"), DedupeVerdict::New);
        assert_eq!(
            dedup.check("a"),
            DedupeVerdict::Duplicate {
                suppressed_count: 1
            }
        );
        assert_eq!(
            dedup.check("b"),
            DedupeVerdict::Duplicate {
                suppressed_count: 1
            }
        );
    }

    #[test]
    fn dedup_expired_key_resets_as_new() {
        let mut dedup = EventDeduplicator::with_config(Duration::from_millis(10), 100);
        dedup.check("key");
        dedup.check("key"); // suppressed_count=1
        std::thread::sleep(Duration::from_millis(20));
        // After expiry, treated as new
        assert_eq!(dedup.check("key"), DedupeVerdict::New);
    }

    #[test]
    fn dedup_suppressed_count_query() {
        let mut dedup = EventDeduplicator::new();
        assert_eq!(dedup.suppressed_count("nope"), 0);
        dedup.check("k");
        assert_eq!(dedup.suppressed_count("k"), 0);
        dedup.check("k");
        assert_eq!(dedup.suppressed_count("k"), 1);
        dedup.check("k");
        assert_eq!(dedup.suppressed_count("k"), 2);
    }

    #[test]
    fn dedup_capacity_eviction() {
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(300), 3);
        dedup.check("a");
        dedup.check("b");
        dedup.check("c");
        assert_eq!(dedup.len(), 3);
        // Adding a 4th evicts the oldest
        dedup.check("d");
        assert_eq!(dedup.len(), 3);
        // "a" was evicted, should be treated as new
        assert_eq!(dedup.check("a"), DedupeVerdict::New);
    }

    // [ft-61kg4] max_capacity=0 must mean "dedup disabled" (every call is
    // New), not "1-slot ghost cache". Before the fix, alternating keys
    // toggled into/out of the single entries slot — producing Duplicate
    // for immediate repeats but New for every cross-key call. Parallel to
    // ft-bx4le in connector_inbound_bridge::SignalDeduplicator.
    #[test]
    fn dedup_zero_capacity_bypasses_dedup_ft_61kg4() {
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(300), 0);

        // Every call — including immediate repeats of the same key — must
        // be treated as New. No state accumulation, no toggling.
        for _ in 0..5 {
            assert_eq!(dedup.check("a"), DedupeVerdict::New);
        }
        // Cross-key calls must also all be New.
        assert_eq!(dedup.check("b"), DedupeVerdict::New);
        assert_eq!(dedup.check("a"), DedupeVerdict::New);
        assert_eq!(dedup.check("c"), DedupeVerdict::New);

        // And no internal state grew — the entries map stays empty, so
        // the deduplicator is not a hidden unbounded-in-practice cache
        // when a caller configures capacity=0.
        assert_eq!(dedup.len(), 0);
        assert!(dedup.is_empty());
        assert_eq!(dedup.suppressed_count("a"), 0);
    }

    #[test]
    fn dedup_entry_timestamps() {
        let mut dedup = EventDeduplicator::new();
        dedup.check("k");
        let entry = dedup.get("k").unwrap();
        assert_eq!(entry.count, 1);
        let first = entry.first_seen;
        let last = entry.last_seen;
        assert!(last >= first);

        std::thread::sleep(Duration::from_millis(5));
        dedup.check("k");
        let entry = dedup.get("k").unwrap();
        assert_eq!(entry.count, 2);
        assert_eq!(entry.first_seen, first);
        assert!(entry.last_seen > last);
    }

    #[test]
    fn dedup_clear_resets() {
        let mut dedup = EventDeduplicator::new();
        dedup.check("a");
        dedup.check("b");
        assert_eq!(dedup.len(), 2);
        dedup.clear();
        assert!(dedup.is_empty());
        assert_eq!(dedup.check("a"), DedupeVerdict::New);
    }

    // Regression: window-expired reset must refresh insertion_order so the
    // entry is not a first-in-line eviction candidate despite being freshly
    // re-activated. Without the refresh, the reset entry stays at its
    // original insertion_order position and gets evicted by the next few
    // novel-key checks; a follow-up check of the same key within its reset
    // window then spuriously returns `New` instead of `Duplicate`.
    #[test]
    fn dedup_window_reset_refreshes_insertion_order_against_capacity_eviction() {
        let mut dedup = EventDeduplicator::with_config(Duration::from_millis(20), 3);

        // Seed "a" at position 0 of insertion_order, then fill to capacity.
        dedup.check("a");
        dedup.check("b");
        dedup.check("c");
        assert_eq!(dedup.len(), 3);

        // Let "a"'s window expire.
        std::thread::sleep(Duration::from_millis(25));

        // Resetting "a" returns New (window expired) AND should move it to
        // the back of the eviction queue. Before the fix, "a" stayed at
        // position 0 and was the next victim.
        assert_eq!(dedup.check("a"), DedupeVerdict::New);

        // Insert a novel key: capacity is full so one entry gets evicted.
        // After the fix, the oldest live entry is "b" (not "a"), so "b"
        // is evicted and "a" survives.
        dedup.check("d");
        assert_eq!(dedup.len(), 3);

        // A follow-up check of "a" within its reset window must still see
        // it as Duplicate. Before the fix, "a" had been evicted by the
        // novel-key eviction pass and this returned New.
        match dedup.check("a") {
            DedupeVerdict::Duplicate { suppressed_count } => {
                assert_eq!(suppressed_count, 1);
            }
            other @ DedupeVerdict::New => panic!(
                "reset-then-check must return Duplicate after insertion_order \
                 refresh; got {other:?}"
            ),
        }

        // "b" (the original second-inserted key) should have been evicted.
        assert_eq!(dedup.check("b"), DedupeVerdict::New);
    }

    // ---- NotificationCooldown tests ----

    #[test]
    fn cooldown_first_occurrence_sends() {
        let mut cd = NotificationCooldown::new();
        assert_eq!(
            cd.check("key"),
            CooldownVerdict::Send {
                suppressed_since_last: 0
            }
        );
    }

    #[test]
    fn cooldown_within_period_suppresses() {
        let mut cd = NotificationCooldown::new();
        cd.check("key");
        assert_eq!(
            cd.check("key"),
            CooldownVerdict::Suppress {
                total_suppressed: 1
            }
        );
    }

    #[test]
    fn cooldown_suppressed_count_increments() {
        let mut cd = NotificationCooldown::new();
        cd.check("k");
        cd.check("k");
        cd.check("k");
        assert_eq!(
            cd.check("k"),
            CooldownVerdict::Suppress {
                total_suppressed: 3
            }
        );
    }

    #[test]
    fn cooldown_expired_sends_with_suppressed_count() {
        let mut cd = NotificationCooldown::with_config(Duration::from_millis(10), 100);
        cd.check("k"); // Send(0)
        cd.check("k"); // Suppress(1)
        cd.check("k"); // Suppress(2)
        std::thread::sleep(Duration::from_millis(20));
        // After cooldown expires, sends with suppressed count
        assert_eq!(
            cd.check("k"),
            CooldownVerdict::Send {
                suppressed_since_last: 2
            }
        );
    }

    #[test]
    fn cooldown_reset_after_send() {
        let mut cd = NotificationCooldown::with_config(Duration::from_millis(10), 100);
        cd.check("k");
        cd.check("k"); // Suppress(1)
        std::thread::sleep(Duration::from_millis(20));
        cd.check("k"); // Send(1) - resets counter
        // Now within cooldown again, suppressed count starts fresh
        assert_eq!(
            cd.check("k"),
            CooldownVerdict::Suppress {
                total_suppressed: 1
            }
        );
    }

    #[test]
    fn cooldown_different_keys_independent() {
        let mut cd = NotificationCooldown::new();
        assert_eq!(
            cd.check("a"),
            CooldownVerdict::Send {
                suppressed_since_last: 0
            }
        );
        assert_eq!(
            cd.check("b"),
            CooldownVerdict::Send {
                suppressed_since_last: 0
            }
        );
        assert_eq!(
            cd.check("a"),
            CooldownVerdict::Suppress {
                total_suppressed: 1
            }
        );
    }

    #[test]
    fn cooldown_capacity_eviction() {
        let mut cd = NotificationCooldown::with_config(Duration::from_secs(300), 3);
        cd.check("a");
        cd.check("b");
        cd.check("c");
        assert_eq!(cd.len(), 3);
        cd.check("d"); // evicts "a"
        assert_eq!(cd.len(), 3);
        // "a" was evicted, treated as new
        assert_eq!(
            cd.check("a"),
            CooldownVerdict::Send {
                suppressed_since_last: 0
            }
        );
    }

    // [ft-hyrav] When a cooldown expires and Send fires, the insertion_order
    // position for that key must be refreshed so the next LRU eviction
    // doesn't pick it over an older-but-truly-dormant key. Pre-fix, the
    // expired-Send branch mutated the entry but left insertion_order
    // alone, so a busy key would be evicted before a dormant one —
    // silently losing its cooldown state and flooding a notification that
    // should have carried a suppressed-count.
    //
    // The test uses a short cooldown so we can observe the expired branch
    // without slow wall-clock sleeps: 5ms cooldown + 50ms sleep is well
    // outside the Instant::now() resolution floor on all supported
    // platforms.
    #[test]
    fn cooldown_expired_send_refreshes_lru_position_ft_hyrav() {
        let mut cd = NotificationCooldown::with_config(Duration::from_millis(5), 3);

        // Seed A, B, C in insertion order. Each is a first-occurrence
        // Send; insertion_order is [A, B, C].
        cd.check("a");
        cd.check("b");
        cd.check("c");
        assert_eq!(cd.len(), 3);

        // Let A's cooldown expire, then re-check. Pre-fix: entry
        // last_notified bumped, but insertion_order stayed [A, B, C].
        // Post-fix: insertion_order becomes [B, C, A].
        std::thread::sleep(Duration::from_millis(50));
        let verdict = cd.check("a");
        assert!(
            matches!(verdict, CooldownVerdict::Send { .. }),
            "A's cooldown expired so check must emit Send, got {verdict:?}"
        );

        // Introduce D to trigger a capacity eviction. Pre-fix: the
        // oldest by INSERTION is A, so A gets evicted even though it
        // was just refreshed. Post-fix: B is now the oldest-by-use,
        // so B is evicted.
        cd.check("d");
        assert_eq!(cd.len(), 3);

        // The key observation: A's state must SURVIVE the D insert.
        // If A was evicted (pre-fix behavior), re-checking A returns
        // Send { suppressed_since_last: 0 } — indistinguishable from
        // a first-occurrence. If A survived (post-fix), the entry is
        // still in cooldown (its last_notified is ~50ms ago, still
        // < 5ms, wait — actually 50ms > 5ms so cooldown has expired
        // again). So we can't detect survival via Suppress alone.
        //
        // Instead, assert the deterministic LRU outcome: B must be
        // the one that's gone. Re-checking B after eviction returns
        // Send with suppressed_since_last=0 (first-occurrence shape),
        // and cd.len() stays at 3.
        assert!(
            cd.get("b").is_none(),
            "ft-hyrav: B must be evicted (oldest-by-use after A was refreshed), but it's still present"
        );
        assert!(
            cd.get("a").is_some(),
            "ft-hyrav: A was just refreshed — it must NOT be the eviction victim"
        );
    }

    #[test]
    fn cooldown_zero_capacity_bypasses_tracking_ft_w80kj() {
        let mut cd = NotificationCooldown::with_config(Duration::from_secs(300), 0);

        for _ in 0..5 {
            assert_eq!(
                cd.check("a"),
                CooldownVerdict::Send {
                    suppressed_since_last: 0,
                }
            );
        }
        assert_eq!(
            cd.check("b"),
            CooldownVerdict::Send {
                suppressed_since_last: 0,
            }
        );
        assert_eq!(
            cd.check("a"),
            CooldownVerdict::Send {
                suppressed_since_last: 0,
            }
        );

        assert_eq!(cd.len(), 0);
        assert!(cd.is_empty());
    }

    #[test]
    fn cooldown_clear_resets() {
        let mut cd = NotificationCooldown::new();
        cd.check("a");
        cd.check("b");
        assert_eq!(cd.len(), 2);
        cd.clear();
        assert!(cd.is_empty());
        assert_eq!(
            cd.check("a"),
            CooldownVerdict::Send {
                suppressed_since_last: 0
            }
        );
    }

    // ========================================================================
    // EventFilter tests (wa-psm.3)
    // ========================================================================

    fn make_detection(
        rule_id: &str,
        severity: crate::patterns::Severity,
        agent_type: crate::patterns::AgentType,
    ) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: "test".to_string(),
            severity,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "test".to_string(),
            span: (0, 4),
        }
    }

    #[test]
    fn truncate_to_char_boundary_avoids_utf8_split_panics() {
        let mut value = format!("{}é", "a".repeat(119));
        assert_eq!(value.len(), 121);

        truncate_to_char_boundary(&mut value, IDENTITY_MAX_VALUE_LEN);
        assert_eq!(value.len(), 119);
        assert!(!value.ends_with('é'));
    }

    #[test]
    fn event_identity_key_handles_multibyte_extracted_values() {
        let mut detection = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        detection.extracted = serde_json::json!({
            "long_text": format!("{}é", "a".repeat(119))
        });

        let key = event_identity_key(&detection, 7, None);
        assert!(key.starts_with("evt:"));
        assert_eq!(key.len(), 68); // "evt:" + 64 hex chars
    }

    #[test]
    fn event_identity_key_ignores_extracted_object_insertion_order() {
        let mut first = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let mut first_map = serde_json::Map::new();
        first_map.insert("agent".to_string(), serde_json::json!("codex"));
        first_map.insert("retry_after".to_string(), serde_json::json!(120));
        first.extracted = serde_json::Value::Object(first_map);

        let mut second = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let mut second_map = serde_json::Map::new();
        second_map.insert("retry_after".to_string(), serde_json::json!(120));
        second_map.insert("agent".to_string(), serde_json::json!("codex"));
        second.extracted = serde_json::Value::Object(second_map);

        assert_eq!(
            event_identity_key(&first, 7, None),
            event_identity_key(&second, 7, None)
        );
    }

    /// [review] Companion to `event_identity_key_ignores_extracted_object_insertion_order`:
    /// 765743d5 sorted the TOP-LEVEL keys of the extracted object, but
    /// nested Object values went through `serde_json::to_string(value)`
    /// which preserves insertion order (serde_json::Map is an IndexMap).
    /// Two logically-equivalent events with nested Objects in different
    /// key orders would still dedup-to-two because their to_string
    /// outputs differ.
    ///
    /// Post-fix: `canonicalize_json_value` recursively sorts all nested
    /// Object keys before serialization, so nested insertion order no
    /// longer leaks into the identity hash.
    #[test]
    fn event_identity_key_ignores_nested_extracted_object_insertion_order() {
        let mut first = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        // Nested "context" object: insertion order a, b.
        let mut first_ctx = serde_json::Map::new();
        first_ctx.insert("a".to_string(), serde_json::json!(1));
        first_ctx.insert("b".to_string(), serde_json::json!(2));
        let mut first_map = serde_json::Map::new();
        first_map.insert("agent".to_string(), serde_json::json!("codex"));
        first_map.insert("context".to_string(), serde_json::Value::Object(first_ctx));
        first.extracted = serde_json::Value::Object(first_map);

        let mut second = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        // Nested "context" object: insertion order b, a (reversed).
        let mut second_ctx = serde_json::Map::new();
        second_ctx.insert("b".to_string(), serde_json::json!(2));
        second_ctx.insert("a".to_string(), serde_json::json!(1));
        let mut second_map = serde_json::Map::new();
        second_map.insert("agent".to_string(), serde_json::json!("codex"));
        second_map.insert("context".to_string(), serde_json::Value::Object(second_ctx));
        second.extracted = serde_json::Value::Object(second_map);

        assert_eq!(
            event_identity_key(&first, 7, None),
            event_identity_key(&second, 7, None),
            "nested object insertion order must not affect identity key"
        );
    }

    /// [review] Arrays stay ordered (JSON arrays are an ordered
    /// sequence, by spec), but Objects inside an Array still get
    /// canonicalized. Verify: two events with an array of objects
    /// whose object keys differ in insertion order must dedup-to-one.
    #[test]
    fn event_identity_key_canonicalizes_objects_inside_arrays() {
        let mut first = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let mut first_obj = serde_json::Map::new();
        first_obj.insert("x".to_string(), serde_json::json!(10));
        first_obj.insert("y".to_string(), serde_json::json!(20));
        let mut first_map = serde_json::Map::new();
        first_map.insert(
            "items".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object(first_obj)]),
        );
        first.extracted = serde_json::Value::Object(first_map);

        let mut second = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let mut second_obj = serde_json::Map::new();
        second_obj.insert("y".to_string(), serde_json::json!(20));
        second_obj.insert("x".to_string(), serde_json::json!(10));
        let mut second_map = serde_json::Map::new();
        second_map.insert(
            "items".to_string(),
            serde_json::Value::Array(vec![serde_json::Value::Object(second_obj)]),
        );
        second.extracted = serde_json::Value::Object(second_map);

        assert_eq!(
            event_identity_key(&first, 7, None),
            event_identity_key(&second, 7, None),
            "array-of-objects: object key order must be canonicalized"
        );
    }

    #[test]
    fn filter_allow_all_passes_everything() {
        let f = EventFilter::allow_all();
        assert!(f.is_permissive());
        let d = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        assert!(f.matches(&d));
    }

    #[test]
    fn filter_include_glob_star() {
        // Pattern "*:usage_*" matches rule_ids with ":usage_" separator
        let f = EventFilter::from_config(&["*:usage_*".to_string()], &[], None, &[]);
        let hit = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let miss = make_detection(
            "core.codex:session_end",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        assert!(f.matches(&hit));
        assert!(!f.matches(&miss));
    }

    #[test]
    fn filter_include_glob_dot_separated() {
        // Pattern "*.error" matches rule_ids like "codex.error"
        let f = EventFilter::from_config(&["*.error".to_string()], &[], None, &[]);
        let hit = make_detection(
            "codex.error",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let miss = make_detection(
            "codex.warning",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        assert!(f.matches(&hit));
        assert!(!f.matches(&miss));
    }

    #[test]
    fn filter_include_exact_match() {
        let f = EventFilter::from_config(&["core.codex:usage_reached".to_string()], &[], None, &[]);
        let hit = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        let miss = make_detection(
            "core.codex:usage_warning",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        assert!(f.matches(&hit));
        assert!(!f.matches(&miss));
    }

    #[test]
    fn filter_exclude_wins_over_include() {
        let f = EventFilter::from_config(
            &["codex.*".to_string()],
            &["codex.debug".to_string()],
            None,
            &[],
        );
        let pass = make_detection(
            "codex.error",
            crate::patterns::Severity::Critical,
            crate::patterns::AgentType::Codex,
        );
        let blocked = make_detection(
            "codex.debug",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        assert!(f.matches(&pass));
        assert!(!f.matches(&blocked));
    }

    #[test]
    fn filter_exclude_glob() {
        let f = EventFilter::from_config(
            &[],
            &["*.debug".to_string(), "test.*".to_string()],
            None,
            &[],
        );
        let blocked1 = make_detection(
            "core.debug",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Unknown,
        );
        let blocked2 = make_detection(
            "test.something",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Unknown,
        );
        let pass = make_detection(
            "core.codex:usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        assert!(!f.matches(&blocked1));
        assert!(!f.matches(&blocked2));
        assert!(f.matches(&pass));
    }

    #[test]
    fn filter_min_severity_info() {
        let f = EventFilter::from_config(&[], &[], Some("info"), &[]);
        let d = make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        assert!(f.matches(&d));
    }

    #[test]
    fn filter_min_severity_warning_blocks_info() {
        let f = EventFilter::from_config(&[], &[], Some("warning"), &[]);
        let info = make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        let warning = make_detection(
            "x",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let critical = make_detection(
            "x",
            crate::patterns::Severity::Critical,
            crate::patterns::AgentType::Codex,
        );
        assert!(!f.matches(&info));
        assert!(f.matches(&warning));
        assert!(f.matches(&critical));
    }

    #[test]
    fn filter_min_severity_critical_blocks_warning() {
        let f = EventFilter::from_config(&[], &[], Some("critical"), &[]);
        let warning = make_detection(
            "x",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        let critical = make_detection(
            "x",
            crate::patterns::Severity::Critical,
            crate::patterns::AgentType::Codex,
        );
        assert!(!f.matches(&warning));
        assert!(f.matches(&critical));
    }

    #[test]
    fn filter_agent_type_allowlist() {
        let f =
            EventFilter::from_config(&[], &[], None, &["codex".to_string(), "gemini".to_string()]);
        let codex = make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );
        let gemini = make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Gemini,
        );
        let claude = make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::ClaudeCode,
        );
        assert!(f.matches(&codex));
        assert!(f.matches(&gemini));
        assert!(!f.matches(&claude));
    }

    #[test]
    fn filter_empty_agent_types_allows_all() {
        let f = EventFilter::from_config(&[], &[], None, &[]);
        let d = make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::ClaudeCode,
        );
        assert!(f.matches(&d));
    }

    #[test]
    fn filter_combined_severity_and_agent() {
        let f = EventFilter::from_config(&[], &[], Some("warning"), &["codex".to_string()]);
        // Codex + Warning → pass
        assert!(f.matches(&make_detection(
            "x",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        )));
        // Codex + Info → blocked by severity
        assert!(!f.matches(&make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        )));
        // Claude + Warning → blocked by agent
        assert!(!f.matches(&make_detection(
            "x",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::ClaudeCode,
        )));
    }

    #[test]
    fn filter_unknown_severity_ignored() {
        let f = EventFilter::from_config(&[], &[], Some("bogus"), &[]);
        // Unknown severity string → min_severity is None → passes
        assert!(f.matches(&make_detection(
            "x",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        )));
    }

    #[test]
    fn filter_question_mark_glob() {
        let f = EventFilter::from_config(&["codex.usage_?eached".to_string()], &[], None, &[]);
        assert!(f.matches(&make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        )));
        assert!(!f.matches(&make_detection(
            "codex.usage_breached",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        )));
    }

    #[test]
    fn filter_default_is_permissive() {
        let f = EventFilter::default();
        assert!(f.is_permissive());
    }

    // ========================================================================
    // NotificationGate tests (wa-psm.3)
    // ========================================================================

    #[test]
    fn gate_first_event_sends() {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(30),
        );
        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        assert_eq!(
            gate.should_notify(&d, 1, None),
            NotifyDecision::Send {
                suppressed_since_last: 0
            }
        );
    }

    #[test]
    fn gate_filtered_event_returns_filtered() {
        let filter = EventFilter::from_config(&[], &["codex.*".to_string()], None, &[]);
        let mut gate = NotificationGate::from_config(
            filter,
            Duration::from_secs(300),
            Duration::from_secs(30),
        );
        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        assert_eq!(gate.should_notify(&d, 1, None), NotifyDecision::Filtered);
    }

    #[test]
    fn gate_dedup_suppresses_repeated() {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_millis(1), // very short cooldown so dedup kicks in first
        );
        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        // First: Send
        assert!(matches!(
            gate.should_notify(&d, 1, None),
            NotifyDecision::Send { .. }
        ));
        // Second: Deduplicated (within 300s dedup window)
        assert!(matches!(
            gate.should_notify(&d, 1, None),
            NotifyDecision::Deduplicated { .. }
        ));
    }

    #[test]
    fn gate_cooldown_throttles_after_dedup_expiry() {
        // Short dedup window, longer cooldown
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_millis(1), // dedup expires fast
            Duration::from_secs(300), // cooldown stays
        );
        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        // First: Send
        assert!(matches!(
            gate.should_notify(&d, 1, None),
            NotifyDecision::Send { .. }
        ));
        // Wait for dedup to expire
        std::thread::sleep(Duration::from_millis(5));
        // Now dedup is expired but cooldown is still active → Throttled
        assert!(matches!(
            gate.should_notify(&d, 1, None),
            NotifyDecision::Throttled { .. }
        ));
    }

    #[test]
    fn gate_different_panes_independent() {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(30),
        );
        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );
        // Pane 1: Send
        assert!(matches!(
            gate.should_notify(&d, 1, None),
            NotifyDecision::Send { .. }
        ));
        // Pane 2: also Send (independent key)
        assert!(matches!(
            gate.should_notify(&d, 2, None),
            NotifyDecision::Send { .. }
        ));
    }

    #[test]
    fn gate_filter_accessor() {
        let filter = EventFilter::from_config(&["test.*".to_string()], &[], None, &[]);
        let gate = NotificationGate::from_config(
            filter,
            Duration::from_secs(300),
            Duration::from_secs(30),
        );
        assert!(!gate.filter().is_permissive());
    }

    // ---- match_rule_glob unit tests ----

    #[test]
    fn glob_exact_match() {
        assert!(match_rule_glob("codex.error", "codex.error"));
        assert!(!match_rule_glob("codex.error", "codex.warn"));
    }

    #[test]
    fn glob_star_suffix() {
        assert!(match_rule_glob("codex.*", "codex.error"));
        assert!(match_rule_glob("codex.*", "codex.warning"));
        assert!(!match_rule_glob("codex.*", "gemini.error"));
    }

    #[test]
    fn glob_star_prefix() {
        assert!(match_rule_glob("*.error", "codex.error"));
        assert!(match_rule_glob("*.error", "gemini.error"));
        assert!(!match_rule_glob("*.error", "codex.warning"));
    }

    #[test]
    fn glob_star_middle() {
        assert!(match_rule_glob(
            "core.*:usage_reached",
            "core.codex:usage_reached"
        ));
        assert!(!match_rule_glob(
            "core.*:usage_reached",
            "core.codex:session_end"
        ));
    }

    #[test]
    fn glob_question_mark() {
        assert!(match_rule_glob("codex.?rror", "codex.error"));
        assert!(!match_rule_glob("codex.?rror", "codex.error2"));
    }

    // ---- severity_level / parse tests ----

    #[test]
    fn severity_level_ordering_batch2() {
        assert!(
            severity_level(crate::patterns::Severity::Info)
                < severity_level(crate::patterns::Severity::Warning)
        );
        assert!(
            severity_level(crate::patterns::Severity::Warning)
                < severity_level(crate::patterns::Severity::Critical)
        );
    }

    #[test]
    fn parse_severity_roundtrip() {
        assert_eq!(
            parse_severity("info"),
            Some(crate::patterns::Severity::Info)
        );
        assert_eq!(
            parse_severity("WARNING"),
            Some(crate::patterns::Severity::Warning)
        );
        assert_eq!(
            parse_severity("Critical"),
            Some(crate::patterns::Severity::Critical)
        );
        assert_eq!(
            parse_severity("InFo"),
            Some(crate::patterns::Severity::Info)
        );
        assert_eq!(parse_severity("unknown"), None);
    }

    #[test]
    fn parse_agent_type_roundtrip() {
        assert_eq!(
            parse_agent_type("codex"),
            Some(crate::patterns::AgentType::Codex)
        );
        assert_eq!(
            parse_agent_type("CLAUDE_CODE"),
            Some(crate::patterns::AgentType::ClaudeCode)
        );
        assert_eq!(
            parse_agent_type("Gemini"),
            Some(crate::patterns::AgentType::Gemini)
        );
        assert_eq!(
            parse_agent_type("wezterm"),
            Some(crate::patterns::AgentType::Wezterm)
        );
        assert_eq!(parse_agent_type("nope"), None);
    }

    // ========================================================================
    // E2E: noise control pipeline (wa-upg.8.6)
    // ========================================================================

    /// E2E: repeated identical events are deduplicated, then cooldown-throttled.
    #[test]
    fn e2e_repeated_events_dedup_then_cooldown() {
        // Short dedup window (1ms) so we can test cooldown after dedup expires.
        // Longer cooldown (300s) to ensure throttling kicks in.
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_millis(1),
            Duration::from_secs(300),
        );
        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );

        // First event: Send
        let r1 = gate.should_notify(&d, 1, None);
        assert_eq!(
            r1,
            NotifyDecision::Send {
                suppressed_since_last: 0
            }
        );

        // Immediate repeat: Deduplicated (within 1ms dedup window)
        let r2 = gate.should_notify(&d, 1, None);
        assert!(
            matches!(r2, NotifyDecision::Deduplicated { .. }),
            "expected Deduplicated, got {r2:?}"
        );

        // Wait for dedup to expire
        std::thread::sleep(Duration::from_millis(5));

        // After dedup expires but within cooldown: Throttled
        let r3 = gate.should_notify(&d, 1, None);
        assert!(
            matches!(r3, NotifyDecision::Throttled { .. }),
            "expected Throttled, got {r3:?}"
        );
    }

    /// E2E: suppressed count escalates correctly across repeated events.
    #[test]
    fn e2e_suppressed_count_escalates() {
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(300), 100);

        // First occurrence: New
        assert_eq!(dedup.check("event_a"), DedupeVerdict::New);

        // Repeat 10 times: count escalates
        for i in 1..=10 {
            assert_eq!(
                dedup.check("event_a"),
                DedupeVerdict::Duplicate {
                    suppressed_count: i
                }
            );
        }

        // Suppressed count reflects total duplicates
        assert_eq!(dedup.suppressed_count("event_a"), 10);
    }

    /// E2E: cooldown tracks suppressed count and reports it on send.
    #[test]
    fn e2e_cooldown_suppressed_count_reported_on_send() {
        let mut cooldown = NotificationCooldown::with_config(Duration::from_millis(10), 100);

        // First: Send (0 suppressed)
        assert_eq!(
            cooldown.check("key_a"),
            CooldownVerdict::Send {
                suppressed_since_last: 0
            }
        );

        // Suppress 3 events within cooldown
        for _ in 0..3 {
            let v = cooldown.check("key_a");
            assert!(matches!(v, CooldownVerdict::Suppress { .. }));
        }

        // Wait for cooldown to expire
        std::thread::sleep(Duration::from_millis(15));

        // Next check: Send with suppressed count = 3
        assert_eq!(
            cooldown.check("key_a"),
            CooldownVerdict::Send {
                suppressed_since_last: 3
            }
        );
    }

    /// E2E: filter excludes events before dedup/cooldown are touched.
    #[test]
    fn e2e_filter_short_circuits_before_dedup() {
        let filter = EventFilter::from_config(&[], &["codex.*".to_string()], None, &[]);
        let mut gate = NotificationGate::from_config(
            filter,
            Duration::from_secs(300),
            Duration::from_secs(30),
        );

        let d = make_detection(
            "codex.usage_reached",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );

        // All attempts are filtered, never reaching dedup
        for _ in 0..5 {
            assert_eq!(gate.should_notify(&d, 1, None), NotifyDecision::Filtered);
        }

        // A different (non-excluded) event still sends fine
        let d2 = make_detection(
            "gemini.session_start",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Gemini,
        );
        assert!(matches!(
            gate.should_notify(&d2, 1, None),
            NotifyDecision::Send { .. }
        ));
    }

    /// E2E: events from different panes are independent in the gate.
    #[test]
    fn e2e_multi_pane_independence() {
        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let d = make_detection(
            "codex.compaction",
            crate::patterns::Severity::Info,
            crate::patterns::AgentType::Codex,
        );

        // Pane 1, 2, 3 all send independently
        for pane_id in 1..=3 {
            let result = gate.should_notify(&d, pane_id, None);
            assert!(
                matches!(result, NotifyDecision::Send { .. }),
                "pane {pane_id} should send, got {result:?}"
            );
        }

        // Repeating on pane 1 is deduplicated
        let result = gate.should_notify(&d, 1, None);
        assert!(matches!(result, NotifyDecision::Deduplicated { .. }));

        // But pane 4 is still new
        let result = gate.should_notify(&d, 4, None);
        assert!(matches!(result, NotifyDecision::Send { .. }));
    }

    /// E2E: mute lifecycle with storage (add, check, list, remove).
    #[test]
    fn e2e_mute_lifecycle_with_storage() {
        run_async_test(async {
            use crate::storage::{EventMuteRecord, StorageHandle};

            let db_path =
                std::env::temp_dir().join(format!("wa_e2e_mute_{}.db", std::process::id()));
            let db_str = db_path.to_string_lossy().to_string();
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(format!("{db_str}-wal"));
            let _ = std::fs::remove_file(format!("{db_str}-shm"));

            let storage = StorageHandle::new(&db_str).await.expect("open test db");
            let now = crate::storage::now_ms();

            // Generate an identity key for our test event
            let d = make_detection(
                "codex.usage_reached",
                crate::patterns::Severity::Warning,
                crate::patterns::AgentType::Codex,
            );
            let identity_key = event_identity_key(&d, 1, None);

            // Initially not muted
            assert!(
                !storage.is_event_muted(&identity_key, now).await.unwrap(),
                "should not be muted initially"
            );

            // Add mute with no expiry (permanent)
            storage
                .add_event_mute(EventMuteRecord {
                    identity_key: identity_key.clone(),
                    scope: "workspace".to_string(),
                    created_at: now,
                    expires_at: None,
                    created_by: Some("test".to_string()),
                    reason: Some("noisy test event".to_string()),
                })
                .await
                .unwrap();

            // Now muted
            assert!(
                storage.is_event_muted(&identity_key, now).await.unwrap(),
                "should be muted after add"
            );

            // Appears in active mutes list
            let mutes = storage.list_active_mutes(now).await.unwrap();
            assert!(
                mutes.iter().any(|m| m.identity_key == identity_key),
                "mute should appear in active list"
            );

            // Remove mute
            storage.remove_event_mute(&identity_key).await.unwrap();

            // No longer muted
            assert!(
                !storage.is_event_muted(&identity_key, now).await.unwrap(),
                "should not be muted after remove"
            );

            // Clean up
            storage.shutdown().await.expect("shutdown");
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(format!("{db_str}-wal"));
            let _ = std::fs::remove_file(format!("{db_str}-shm"));
        });
    }

    /// E2E: mute with expiry automatically expires.
    #[test]
    fn e2e_mute_expiry() {
        run_async_test(async {
            use crate::storage::{EventMuteRecord, StorageHandle};

            let db_path =
                std::env::temp_dir().join(format!("wa_e2e_mute_expiry_{}.db", std::process::id()));
            let db_str = db_path.to_string_lossy().to_string();
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(format!("{db_str}-wal"));
            let _ = std::fs::remove_file(format!("{db_str}-shm"));

            let storage = StorageHandle::new(&db_str).await.expect("open test db");
            let now = crate::storage::now_ms();

            let identity_key = "evt:test_expiry_key".to_string();

            // Add mute that already expired (expires_at in the past)
            storage
                .add_event_mute(EventMuteRecord {
                    identity_key: identity_key.clone(),
                    scope: "workspace".to_string(),
                    created_at: now - 60_000,
                    expires_at: Some(now - 1000), // expired 1 second ago
                    created_by: None,
                    reason: None,
                })
                .await
                .unwrap();

            // Should not be active since it's expired
            assert!(
                !storage.is_event_muted(&identity_key, now).await.unwrap(),
                "expired mute should not be active"
            );

            // Should not appear in active mutes list
            let mutes = storage.list_active_mutes(now).await.unwrap();
            assert!(
                !mutes.iter().any(|m| m.identity_key == identity_key),
                "expired mute should not appear in active list"
            );

            // Add a mute that expires in the future
            let future_key = "evt:test_future_key".to_string();
            storage
                .add_event_mute(EventMuteRecord {
                    identity_key: future_key.clone(),
                    scope: "workspace".to_string(),
                    created_at: now,
                    expires_at: Some(now + 3_600_000), // 1 hour from now
                    created_by: None,
                    reason: Some("temporary mute".to_string()),
                })
                .await
                .unwrap();

            // Should be active
            assert!(
                storage.is_event_muted(&future_key, now).await.unwrap(),
                "future mute should be active"
            );

            // Clean up
            storage.shutdown().await.expect("shutdown");
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(format!("{db_str}-wal"));
            let _ = std::fs::remove_file(format!("{db_str}-shm"));
        });
    }

    /// E2E: full noise control pipeline - dedup → cooldown → mute check.
    #[test]
    fn e2e_full_pipeline_dedup_cooldown_mute() {
        run_async_test(async {
            use crate::storage::{EventMuteRecord, StorageHandle};

            let db_path = std::env::temp_dir()
                .join(format!("wa_e2e_full_pipeline_{}.db", std::process::id()));
            let db_str = db_path.to_string_lossy().to_string();
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(format!("{db_str}-wal"));
            let _ = std::fs::remove_file(format!("{db_str}-shm"));

            let storage = StorageHandle::new(&db_str).await.expect("open test db");
            let now = crate::storage::now_ms();

            let d = make_detection(
                "codex.compaction",
                crate::patterns::Severity::Warning,
                crate::patterns::AgentType::Codex,
            );
            let identity_key = event_identity_key(&d, 1, None);

            // Step 1: gate allows first event
            let mut gate = NotificationGate::from_config(
                EventFilter::allow_all(),
                Duration::from_secs(300),
                Duration::from_secs(300),
            );
            let r1 = gate.should_notify(&d, 1, None);
            assert!(matches!(r1, NotifyDecision::Send { .. }));

            // Step 2: gate deduplicates second event
            let r2 = gate.should_notify(&d, 1, None);
            assert!(matches!(r2, NotifyDecision::Deduplicated { .. }));

            // Step 3: mute the event via storage
            storage
                .add_event_mute(EventMuteRecord {
                    identity_key: identity_key.clone(),
                    scope: "workspace".to_string(),
                    created_at: now,
                    expires_at: None,
                    created_by: Some("operator".to_string()),
                    reason: Some("too noisy".to_string()),
                })
                .await
                .unwrap();

            // Step 4: verify mute is active
            assert!(storage.is_event_muted(&identity_key, now).await.unwrap());

            // Step 5: muted event is visible in muted list
            let mutes = storage.list_active_mutes(now).await.unwrap();
            let our_mute = mutes.iter().find(|m| m.identity_key == identity_key);
            assert!(our_mute.is_some(), "muted event should be in list");
            assert_eq!(our_mute.unwrap().reason.as_deref(), Some("too noisy"));

            // Step 6: after unmuting, is_event_muted returns false
            storage.remove_event_mute(&identity_key).await.unwrap();
            assert!(!storage.is_event_muted(&identity_key, now).await.unwrap());

            // Clean up
            storage.shutdown().await.expect("shutdown");
            let _ = std::fs::remove_file(&db_path);
            let _ = std::fs::remove_file(format!("{db_str}-wal"));
            let _ = std::fs::remove_file(format!("{db_str}-shm"));
        });
    }

    /// E2E: dedup and cooldown JSON artifacts are deterministic.
    #[test]
    fn e2e_noise_control_json_stability() {
        let d = make_detection(
            "codex.compaction",
            crate::patterns::Severity::Warning,
            crate::patterns::AgentType::Codex,
        );

        let mut gate = NotificationGate::from_config(
            EventFilter::allow_all(),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );

        // Collect decisions
        let decisions: Vec<NotifyDecision> =
            (0..5).map(|_| gate.should_notify(&d, 1, None)).collect();

        // First is Send, rest are Deduplicated
        assert!(matches!(decisions[0], NotifyDecision::Send { .. }));
        for decision in &decisions[1..] {
            assert!(matches!(decision, NotifyDecision::Deduplicated { .. }));
        }

        // Suppressed counts are monotonically increasing
        let counts: Vec<u64> = decisions[1..]
            .iter()
            .map(|d| match d {
                NotifyDecision::Deduplicated { suppressed_count } => *suppressed_count,
                _ => panic!("expected Deduplicated"),
            })
            .collect();
        for window in counts.windows(2) {
            assert!(
                window[1] > window[0],
                "suppressed counts should increase monotonically: {counts:?}"
            );
        }
    }

    // =========================================================================
    // Batch: DarkBadger wa-1u90p.7.1 — trait & edge coverage
    // =========================================================================

    // --- UserVarPayload ---

    #[test]
    fn user_var_payload_debug_clone_serde() {
        let p = UserVarPayload {
            value: "test".to_string(),
            event_type: Some("cmd".to_string()),
            event_data: Some(serde_json::json!({"key": "val"})),
        };
        let cloned = p.clone();
        assert_eq!(cloned.value, "test");
        assert_eq!(cloned.event_type.as_deref(), Some("cmd"));
        let dbg = format!("{:?}", p);
        assert!(dbg.contains("UserVarPayload"));

        let json = serde_json::to_string(&p).unwrap();
        let parsed: UserVarPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.value, "test");
        assert_eq!(parsed.event_type.as_deref(), Some("cmd"));
    }

    // --- UserVarError ---

    #[test]
    fn user_var_error_debug_clone() {
        let e = UserVarError::WatcherNotRunning {
            socket_path: "/tmp/test.sock".to_string(),
        };
        let cloned = e.clone();
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("WatcherNotRunning"));
    }

    #[test]
    fn user_var_error_display_all_variants() {
        let e1 = UserVarError::WatcherNotRunning {
            socket_path: "/tmp/x.sock".to_string(),
        };
        assert!(e1.to_string().contains("/tmp/x.sock"));

        let e2 = UserVarError::IpcSendFailed {
            message: "timeout".to_string(),
        };
        assert!(e2.to_string().contains("timeout"));

        let e3 = UserVarError::ParseFailed("bad data".to_string());
        assert!(e3.to_string().contains("bad data"));
    }

    #[test]
    fn user_var_error_is_std_error() {
        let e = UserVarError::ParseFailed("test".to_string());
        let _: &dyn std::error::Error = &e;
    }

    // --- Event ---

    #[test]
    fn event_type_name_all_nine_variants() {
        let events: Vec<(Event, &str)> = vec![
            (
                Event::SegmentCaptured {
                    pane_id: 1,
                    seq: 0,
                    content_len: 0,
                },
                "segment_captured",
            ),
            (
                Event::GapDetected {
                    pane_id: 1,
                    seq_before: 4,
                    seq_after: 5,
                    reason: "x".into(),
                    detected_at_ms: 1234,
                },
                "gap_detected",
            ),
            (
                Event::PatternDetected {
                    pane_id: 1,
                    pane_uuid: None,
                    detection: make_detection(
                        "test.rule",
                        crate::patterns::Severity::Info,
                        crate::patterns::AgentType::Codex,
                    ),
                    event_id: None,
                },
                "pattern_detected",
            ),
            (
                Event::PaneDiscovered {
                    pane_id: 1,
                    domain: "d".into(),
                    title: "t".into(),
                },
                "pane_discovered",
            ),
            (Event::PaneDisappeared { pane_id: 1 }, "pane_disappeared"),
            (
                Event::WorkflowStarted {
                    workflow_id: "w".into(),
                    workflow_name: "n".into(),
                    pane_id: 1,
                },
                "workflow_started",
            ),
            (
                Event::WorkflowStep {
                    workflow_id: "w".into(),
                    step_name: "s".into(),
                    result: "ok".into(),
                },
                "workflow_step",
            ),
            (
                Event::WorkflowCompleted {
                    workflow_id: "w".into(),
                    success: false,
                    reason: Some("fail".into()),
                },
                "workflow_completed",
            ),
            (
                Event::UserVarReceived {
                    pane_id: 1,
                    name: "FT_EVENT".into(),
                    payload: UserVarPayload {
                        value: "x".into(),
                        event_type: None,
                        event_data: None,
                    },
                },
                "user_var_received",
            ),
        ];
        for (event, expected_name) in &events {
            assert_eq!(
                event.type_name(),
                *expected_name,
                "type_name mismatch for {:?}",
                expected_name
            );
        }
    }

    #[test]
    fn event_pane_id_all_variants() {
        // Variants with pane_id
        assert_eq!(
            Event::SegmentCaptured {
                pane_id: 42,
                seq: 0,
                content_len: 0
            }
            .pane_id(),
            Some(42)
        );
        assert_eq!(Event::PaneDisappeared { pane_id: 99 }.pane_id(), Some(99));
        // Variants without pane_id
        assert_eq!(
            Event::WorkflowStep {
                workflow_id: "w".into(),
                step_name: "s".into(),
                result: "r".into()
            }
            .pane_id(),
            None
        );
        assert_eq!(
            Event::WorkflowCompleted {
                workflow_id: "w".into(),
                success: false,
                reason: Some("fail".into())
            }
            .pane_id(),
            None
        );
    }

    #[test]
    fn event_serde_segment_captured_roundtrip() {
        let e = Event::SegmentCaptured {
            pane_id: 1,
            seq: 42,
            content_len: 100,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("\"type\":\"segment_captured\""));
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.type_name(), "segment_captured");
    }

    #[test]
    fn event_serde_workflow_completed_with_reason() {
        let e = Event::WorkflowCompleted {
            workflow_id: "wf-1".into(),
            success: false,
            reason: Some("timeout".into()),
        };
        let json = serde_json::to_string(&e).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        if let Event::WorkflowCompleted { reason, .. } = parsed {
            assert_eq!(reason.as_deref(), Some("timeout"));
        } else {
            panic!("wrong variant");
        }
    }

    // --- MetricsSnapshot ---

    #[test]
    fn metrics_snapshot_debug_clone_serde() {
        let snap = MetricsSnapshot {
            events_published: 100,
            events_dropped_no_subscribers: 5,
            // br-ft-8cyii sibling-cleanup.
            events_dropped_dedup: 0,
            // br-ft-2z16v.
            events_delivered: 95,
            active_subscribers: 3,
            subscriber_lag_events: 2,
            // br-ft-skec1.
            bus_lock_poisoned_count: 0,
            // br-ft-tpdl5.
            delta_dedup_full_count: 0,
        };
        let cloned = snap.clone();
        assert_eq!(cloned.events_published, 100);
        let dbg = format!("{:?}", snap);
        assert!(dbg.contains("MetricsSnapshot"));

        let json = serde_json::to_string(&snap).unwrap();
        let parsed: MetricsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.events_published, 100);
        assert_eq!(parsed.active_subscribers, 3);
    }

    // --- EventBusStats ---

    #[test]
    fn event_bus_stats_debug_clone_serde() {
        let stats = EventBusStats {
            capacity: 1000,
            delta_queued: 10,
            detection_queued: 5,
            signal_queued: 2,
            delta_subscribers: 3,
            detection_subscribers: 1,
            signal_subscribers: 0,
            delta_oldest_lag_ms: Some(500),
            detection_oldest_lag_ms: None,
            signal_oldest_lag_ms: None,
            delta_dedup: EventCuckooDedup::default().snapshot(),
            causality: EventCausalityClock::default().snapshot(),
        };
        let cloned = stats.clone();
        assert_eq!(cloned.capacity, 1000);
        let dbg = format!("{:?}", stats);
        assert!(dbg.contains("EventBusStats"));

        let json = serde_json::to_string(&stats).unwrap();
        let parsed: EventBusStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.delta_oldest_lag_ms, Some(500));
        assert_eq!(parsed.detection_oldest_lag_ms, None);
    }

    // --- EventBusMetrics ---

    #[test]
    fn event_bus_metrics_new_equals_default() {
        let a = EventBusMetrics::new();
        let b = EventBusMetrics::default();
        assert_eq!(
            a.events_published.load(Ordering::Relaxed),
            b.events_published.load(Ordering::Relaxed)
        );
        assert_eq!(
            a.active_subscribers.load(Ordering::Relaxed),
            b.active_subscribers.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn event_bus_metrics_snapshot_reflects_increments() {
        let m = EventBusMetrics::new();
        m.events_published.fetch_add(10, Ordering::Relaxed);
        m.subscriber_lag_events.fetch_add(3, Ordering::Relaxed);
        let snap = m.snapshot();
        assert_eq!(snap.events_published, 10);
        assert_eq!(snap.subscriber_lag_events, 3);
    }

    // --- RecvError ---

    #[test]
    fn recv_error_debug_clone() {
        let e = RecvError::Lagged { missed_count: 42 };
        let cloned = e.clone();
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("Lagged"));
        assert!(dbg.contains("42"));
    }

    #[test]
    fn recv_error_display_both_variants() {
        let closed = RecvError::Closed;
        assert_eq!(closed.to_string(), "event bus closed");

        let cancelled = RecvError::Cancelled;
        assert_eq!(cancelled.to_string(), "event subscriber cancelled");

        let lagged = RecvError::Lagged { missed_count: 5 };
        assert!(lagged.to_string().contains("missed 5 events"));
    }

    #[test]
    fn recv_error_is_std_error() {
        let e = RecvError::Closed;
        let _: &dyn std::error::Error = &e;
    }

    // --- DedupeVerdict ---

    #[test]
    fn dedupe_verdict_debug_clone_eq() {
        let v = DedupeVerdict::New;
        let cloned = v.clone();
        assert_eq!(v, cloned);
        assert_ne!(
            DedupeVerdict::New,
            DedupeVerdict::Duplicate {
                suppressed_count: 0
            }
        );
        assert_eq!(
            DedupeVerdict::Duplicate {
                suppressed_count: 3
            },
            DedupeVerdict::Duplicate {
                suppressed_count: 3
            }
        );
        assert_ne!(
            DedupeVerdict::Duplicate {
                suppressed_count: 1
            },
            DedupeVerdict::Duplicate {
                suppressed_count: 2
            }
        );
    }

    // --- CooldownVerdict ---

    #[test]
    fn cooldown_verdict_debug_clone_eq() {
        let s = CooldownVerdict::Send {
            suppressed_since_last: 0,
        };
        let cloned = s.clone();
        assert_eq!(s, cloned);

        let sup = CooldownVerdict::Suppress {
            total_suppressed: 5,
        };
        assert_ne!(s, sup);
        let dbg = format!("{:?}", sup);
        assert!(dbg.contains("Suppress"));
    }

    // --- EventDeduplicator ---

    #[test]
    fn event_deduplicator_debug_clone() {
        let d = EventDeduplicator::new();
        let cloned = d.clone();
        assert_eq!(cloned.len(), 0);
        assert!(cloned.is_empty());
        let dbg = format!("{:?}", d);
        assert!(dbg.contains("EventDeduplicator"));
    }

    #[test]
    fn event_deduplicator_default_equals_new() {
        let a = EventDeduplicator::new();
        let b = EventDeduplicator::default();
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn event_deduplicator_clone_independence() {
        let mut d = EventDeduplicator::new();
        d.check("key1");
        let mut cloned = d.clone();
        cloned.check("key2");
        assert_eq!(d.len(), 1);
        assert_eq!(cloned.len(), 2);
    }

    #[test]
    fn event_deduplicator_clear_resets() {
        let mut d = EventDeduplicator::new();
        d.check("a");
        d.check("b");
        assert_eq!(d.len(), 2);
        d.clear();
        assert!(d.is_empty());
    }

    // --- NotificationCooldown ---

    #[test]
    fn notification_cooldown_debug_clone() {
        let c = NotificationCooldown::new();
        let cloned = c.clone();
        assert_eq!(cloned.len(), 0);
        assert!(cloned.is_empty());
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("NotificationCooldown"));
    }

    #[test]
    fn notification_cooldown_default_equals_new() {
        let a = NotificationCooldown::new();
        let b = NotificationCooldown::default();
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn notification_cooldown_clear_resets() {
        let mut c = NotificationCooldown::new();
        c.check("k");
        assert_eq!(c.len(), 1);
        c.clear();
        assert!(c.is_empty());
    }

    // --- EventFilter ---

    #[test]
    fn event_filter_debug_clone() {
        let f = EventFilter::allow_all();
        let cloned = f.clone();
        let dbg = format!("{:?}", cloned);
        assert!(dbg.contains("EventFilter"));
    }

    // --- severity_level ordering ---

    #[test]
    fn severity_level_ordering_v2() {
        use crate::patterns::Severity;
        assert!(severity_level(Severity::Info) < severity_level(Severity::Warning));
        assert!(severity_level(Severity::Warning) < severity_level(Severity::Critical));
        assert_eq!(severity_level(Severity::Info), 0);
        assert_eq!(severity_level(Severity::Warning), 1);
        assert_eq!(severity_level(Severity::Critical), 2);
    }

    // --- parse_severity ---

    #[test]
    fn parse_severity_case_insensitive() {
        use crate::patterns::Severity;
        assert_eq!(parse_severity("INFO"), Some(Severity::Info));
        assert_eq!(parse_severity("Warning"), Some(Severity::Warning));
        assert_eq!(parse_severity("CRITICAL"), Some(Severity::Critical));
        assert_eq!(parse_severity("InFo"), Some(Severity::Info));
        assert_eq!(parse_severity("unknown"), None);
    }

    // --- parse_agent_type ---

    #[test]
    fn parse_agent_type_case_insensitive() {
        use crate::patterns::AgentType;
        assert_eq!(parse_agent_type("CODEX"), Some(AgentType::Codex));
        assert_eq!(parse_agent_type("Claude_Code"), Some(AgentType::ClaudeCode));
        assert_eq!(parse_agent_type("GEMINI"), Some(AgentType::Gemini));
        assert_eq!(parse_agent_type("nope"), None);
    }

    // --- match_rule_glob ---

    #[test]
    fn match_rule_glob_exact_no_wildcard() {
        assert!(match_rule_glob("codex.error", "codex.error"));
        assert!(!match_rule_glob("codex.error", "codex.warn"));
    }

    #[test]
    fn match_rule_glob_star_prefix() {
        assert!(match_rule_glob("*.error", "codex.error"));
        assert!(!match_rule_glob("*.error", "codex.warning"));
        assert!(match_rule_glob("*.error", "gemini.error"));
    }

    #[test]
    fn match_rule_glob_question_mark() {
        assert!(match_rule_glob("codex.err?r", "codex.error"));
        // `?` matches any single char, so codex.err?r matches codex.errrr
        assert!(match_rule_glob("codex.err?r", "codex.errrr"));
        // But it should NOT match a longer string
        assert!(!match_rule_glob("codex.err?r", "codex.errror"));
    }

    // ─── br-ft-8cyii: dedup-drop counter forensic invariant ──────────────
    //
    // Pin the operator-visibility contract. The forensic invariant:
    //
    //     events_published == delivered_to_subscribers
    //                          + events_dropped_no_subscribers
    //                          + events_dropped_dedup
    //
    // Same shape as ft-luav8 (record_mcp_audit silent-failure
    // counter): silent state loss + observable counter.

    #[test]
    fn events_dropped_dedup_increments_on_duplicate_delta_publish() {
        let bus = EventBus::new(8);
        let _sub = bus.subscribe(); // ensure delivered > 0
        // Two identical SegmentCaptured events back-to-back —
        // the second triggers the cuckoo-dedup early-return.
        let evt = Event::SegmentCaptured {
            pane_id: 1,
            seq: 42,
            content_len: 100,
        };
        let delivered_first = bus.publish(evt.clone());
        let delivered_second = bus.publish(evt);
        let snap = bus.metrics.snapshot();
        // First publish delivered to ≥ 1 subscriber; second was
        // dedup-dropped (delivered=0 + counter bumped).
        assert!(
            delivered_first >= 1,
            "first publish must reach the subscriber (got {delivered_first})"
        );
        assert_eq!(
            delivered_second, 0,
            "duplicate delta must short-circuit at dedup gate (got delivered={delivered_second})"
        );
        assert_eq!(
            snap.events_published, 2,
            "both publishes count toward published"
        );
        assert!(
            snap.events_dropped_dedup >= 1,
            "br-ft-8cyii: dedup-drop counter must increment on duplicate delta \
             (got {})",
            snap.events_dropped_dedup,
        );
    }

    #[test]
    fn events_dropped_dedup_zero_for_non_delta_events() {
        // Non-delta events (PaneDiscovered, etc.) are not subject
        // to the cuckoo-dedup gate. Counter should NOT increment.
        // Sibling cleanup: PaneDiscovered variant no longer carries
        // window_id/tab_id/generation — only pane_id, domain, title.
        let bus = EventBus::new(8);
        let _sub = bus.subscribe();
        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "shell".to_string(),
        });
        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "shell".to_string(),
        });
        let snap = bus.metrics.snapshot();
        assert_eq!(
            snap.events_dropped_dedup, 0,
            "non-delta events bypass the dedup gate; counter must stay 0",
        );
    }

    #[test]
    fn metrics_snapshot_serde_includes_events_dropped_dedup() {
        // Pin the wire shape: the new field must serialize +
        // deserialize cleanly. `#[serde(default)]` lets old
        // snapshots without the field still parse (forward-compat).
        let bus = EventBus::new(4);
        let snap = bus.metrics.snapshot();
        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        assert!(
            json.contains("events_dropped_dedup"),
            "snapshot JSON must include events_dropped_dedup field; got {json}"
        );
        let parsed: MetricsSnapshot = serde_json::from_str(&json).expect("snapshot deserializes");
        assert_eq!(parsed.events_dropped_dedup, snap.events_dropped_dedup);
    }

    #[test]
    fn metrics_snapshot_deserialize_old_format_without_dedup_field() {
        // Forward-compat: a snapshot serialized before br-ft-8cyii
        // (no events_dropped_dedup field) must still deserialize.
        // `#[serde(default)]` makes the field default to 0.
        // br-ft-2z16v: events_delivered also serde-defaults.
        let old_json = r#"{
            "events_published": 100,
            "events_dropped_no_subscribers": 5,
            "active_subscribers": 3,
            "subscriber_lag_events": 2
        }"#;
        let parsed: MetricsSnapshot =
            serde_json::from_str(old_json).expect("old format must still deserialize");
        assert_eq!(parsed.events_published, 100);
        assert_eq!(
            parsed.events_dropped_dedup, 0,
            "missing field defaults to 0"
        );
        assert_eq!(parsed.events_delivered, 0, "missing field defaults to 0");
    }

    // ─── br-ft-2z16v: events_delivered + closed forensic invariant ───
    //
    // Pin the corrected invariant in event-units (not fanout):
    //
    //     events_published == events_delivered
    //                          + events_dropped_no_subscribers
    //                          + events_dropped_dedup
    //
    // Both sides count distinct events; identity holds for every
    // subscriber topology (the pre-fix invariant used `delivered`
    // which is fanout — failed for N-subscriber configurations).

    #[test]
    fn events_delivered_increments_once_per_event_with_subscribers() {
        let bus = EventBus::new(8);
        let _sub = bus.subscribe(); // 1 all-subscriber

        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 10,
        });

        let snap = bus.metrics.snapshot();
        assert_eq!(snap.events_published, 1);
        assert_eq!(
            snap.events_delivered, 1,
            "br-ft-2z16v: events_delivered counts events, not fanout — \
             one published event with one subscriber bumps by exactly 1"
        );
        assert_eq!(snap.events_dropped_no_subscribers, 0);
        assert_eq!(snap.events_dropped_dedup, 0);
    }

    #[test]
    fn events_delivered_unchanged_when_zero_subscribers() {
        let bus = EventBus::new(8);
        // No subscribers — every publish hits the no-subscribers path.
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 10,
        });

        let snap = bus.metrics.snapshot();
        assert_eq!(snap.events_published, 1);
        assert_eq!(
            snap.events_delivered, 0,
            "br-ft-2z16v: zero-subscriber publish must NOT bump events_delivered"
        );
        assert_eq!(snap.events_dropped_no_subscribers, 1);
    }

    #[test]
    fn events_delivered_unchanged_when_dedup_drops_event() {
        let bus = EventBus::new(8);
        let _sub = bus.subscribe();
        let evt = Event::SegmentCaptured {
            pane_id: 1,
            seq: 42,
            content_len: 100,
        };
        // First publish reaches the subscriber.
        let _ = bus.publish(evt.clone());
        // Second publish is dedup-dropped before fanout.
        let _ = bus.publish(evt);

        let snap = bus.metrics.snapshot();
        assert_eq!(snap.events_published, 2);
        assert_eq!(
            snap.events_delivered, 1,
            "br-ft-2z16v: dedup-dropped events must NOT bump events_delivered"
        );
        assert!(snap.events_dropped_dedup >= 1);
    }

    #[test]
    fn events_delivered_counts_event_not_fanout_with_multiple_subscribers() {
        let bus = EventBus::new(8);
        // Three independent subscribers on the all-channel.
        let _sub_a = bus.subscribe();
        let _sub_b = bus.subscribe();
        let _sub_c = bus.subscribe();
        // One delta-channel subscriber too — fanout doubles for
        // SegmentCaptured (all_sender + delta_sender).
        let _sub_d = bus.subscribe_deltas();

        let delivered = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 10,
        });

        // Fanout: 3 (all) + 1 (delta) = 4 subscriber-receptions.
        assert!(
            delivered >= 4,
            "publish() return value is fanout (got {delivered}); pre-fix \
             docstring confused this with event count"
        );
        let snap = bus.metrics.snapshot();
        assert_eq!(snap.events_published, 1);
        assert_eq!(
            snap.events_delivered, 1,
            "br-ft-2z16v: events_delivered counts EVENTS, not fanout — \
             one event with 4 subscriber-receptions still bumps by exactly 1"
        );
    }

    #[test]
    fn forensic_invariant_holds_across_mixed_publish_sequence() {
        // The closed forensic invariant — assert it holds after a
        // mixed sequence of zero-subscriber, one-subscriber, and
        // dedup-dropped publishes.
        let bus = EventBus::new(16);

        // Phase 1: publish two events with no subscribers.
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 1,
            seq: 1,
            content_len: 10,
        });
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 2,
            seq: 1,
            content_len: 20,
        });

        // Phase 2: subscribe; publish three deliverable events.
        let _sub = bus.subscribe();
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 3,
            seq: 1,
            content_len: 30,
        });
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 4,
            seq: 1,
            content_len: 40,
        });
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 5,
            seq: 1,
            content_len: 50,
        });

        // Phase 3: republish one of the deliverable events to
        // trigger the dedup gate.
        let _ = bus.publish(Event::SegmentCaptured {
            pane_id: 3,
            seq: 1,
            content_len: 30,
        });

        let snap = bus.metrics.snapshot();
        assert_eq!(
            snap.events_published, 6,
            "all six publish() calls must count toward events_published"
        );

        // br-ft-2z16v closed forensic invariant.
        let lhs = snap.events_published;
        let rhs =
            snap.events_delivered + snap.events_dropped_no_subscribers + snap.events_dropped_dedup;
        assert_eq!(
            lhs, rhs,
            "br-ft-2z16v: forensic invariant must hold — \
             events_published ({lhs}) == events_delivered ({}) + \
             events_dropped_no_subscribers ({}) + events_dropped_dedup ({}) = {rhs}",
            snap.events_delivered, snap.events_dropped_no_subscribers, snap.events_dropped_dedup,
        );
    }

    // ─── br-ft-tpdl5: cuckoo saturation counter ──────────────────────
    //
    // The cuckoo dedup filter is fixed-capacity (DEFAULT_CAPACITY=2000).
    // Past saturation (load_factor >= 0.95) the underlying insert silently
    // fails, so newly-seen keys are never recorded — first observation
    // is `New`, but every subsequent observation of the same key is ALSO
    // `New` (effective dedup is disabled for post-saturation keys).
    //
    // The counter at EventBusMetrics::delta_dedup_full_count surfaces this
    // silent-disable so operators can:
    //   1. Detect the gate is degraded (counter > 0).
    //   2. Distinguish "no duplicates were filtered" from "the filter
    //      stopped catching them".

    #[test]
    fn delta_dedup_full_count_zero_for_low_volume_traffic() {
        // Sub-saturation traffic must NOT bump the counter — the
        // gate is operating normally.
        let bus = EventBus::new(8);
        let _sub = bus.subscribe();
        for seq in 0..10 {
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq,
                content_len: 10,
            });
        }

        let snap = bus.metrics.snapshot();
        assert_eq!(
            snap.delta_dedup_full_count, 0,
            "br-ft-tpdl5: low-volume traffic must NOT bump the saturation \
             counter (10 events << 2000 capacity)"
        );
    }

    #[test]
    fn delta_dedup_full_count_increments_at_saturation() {
        // Drive the cuckoo filter past 0.95 load_factor. Subsequent
        // is_duplicate_delta_event calls must observe saturation
        // and bump the counter exactly once per call.
        //
        // DEFAULT_CAPACITY=2000; threshold 0.95 → ~1900 distinct keys
        // before saturation is observed. Use 2200 distinct events to
        // guarantee post-saturation observations.
        let bus = EventBus::new(8);
        let _sub = bus.subscribe();
        for seq in 0..2200 {
            let _ = bus.publish(Event::SegmentCaptured {
                pane_id: 1,
                seq,
                content_len: 10,
            });
        }

        let snap = bus.metrics.snapshot();
        assert!(
            snap.delta_dedup_full_count > 0,
            "br-ft-tpdl5: 2200 distinct keys must drive the cuckoo filter \
             past the 0.95 saturation threshold; counter must be > 0 \
             (got {})",
            snap.delta_dedup_full_count,
        );
        // Sanity: the counter is bounded by the number of dedup-check
        // calls, which is at most the number of published delta events.
        assert!(
            snap.delta_dedup_full_count <= snap.events_published,
            "saturation counter ({}) exceeded events_published ({}) — \
             counter bookkeeping is wrong",
            snap.delta_dedup_full_count,
            snap.events_published,
        );
    }

    #[test]
    fn delta_dedup_full_count_serde_roundtrips_via_metrics_snapshot() {
        // Pin the wire shape: the new counter must serialize and
        // deserialize cleanly. `#[serde(default)]` lets old
        // snapshots without the field still parse (forward-compat).
        let bus = EventBus::new(4);
        let snap = bus.metrics.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize");
        assert!(
            json.contains("delta_dedup_full_count"),
            "snapshot JSON must include delta_dedup_full_count; got {json}"
        );
        let parsed: MetricsSnapshot = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.delta_dedup_full_count, snap.delta_dedup_full_count);

        // Forward-compat: old snapshots without the field default to 0.
        let old_json = r#"{
            "events_published": 100,
            "events_dropped_no_subscribers": 5,
            "active_subscribers": 3,
            "subscriber_lag_events": 2
        }"#;
        let parsed: MetricsSnapshot =
            serde_json::from_str(old_json).expect("old format deserialize");
        assert_eq!(parsed.delta_dedup_full_count, 0);
    }

    // ── [ft-s6l5b] match_rule_glob property tests ────────────────────
    //
    // The hand-rolled glob matcher at events.rs:1121 is a backtrack
    // state machine with `*` and `?` wildcards. The preceding example
    // tests pin specific cases; these properties pin the CONTRACT the
    // state machine must satisfy across random inputs — a regression
    // in any of these breaks exclude-rule correctness for operators.

    // Fixture alphabet: the rule-id character set in practice is
    // ASCII alphanumerics + `.`, `:`, `_`. Confining proptest inputs
    // to this alphabet keeps the state machine exercised on realistic
    // traffic without wasting shrinks on bytes the grammar will never
    // see (emoji, UTF-8 multi-byte, etc.). `?` and `*` are excluded
    // from the VALUE alphabet so we can distinguish "pattern wildcard"
    // from "literal asterisk in value" cleanly.
    fn arb_exact_value() -> impl proptest::prelude::Strategy<Value = String> {
        use proptest::prelude::*;
        prop::collection::vec(
            prop_oneof![
                9 => (b'a'..=b'z').prop_map(|b| b as char),
                2 => (b'0'..=b'9').prop_map(|b| b as char),
                1 => Just('.'),
                1 => Just(':'),
                1 => Just('_'),
            ],
            0..24,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    proptest::proptest! {
        /// Invariant 1 — exact patterns (no `*`, no `?`) are idempotent
        /// under self-match AND reject any value of different length.
        /// Pins the fast-exit branch at events.rs:1122 that short-
        /// circuits on non-wildcard patterns.
        #[test]
        fn match_rule_glob_exact_pattern_self_matches(pat in arb_exact_value()) {
            proptest::prop_assume!(!pat.contains('*') && !pat.contains('?'));
            proptest::prop_assert!(
                match_rule_glob(&pat, &pat),
                "exact pattern {pat:?} must match itself"
            );
        }

        /// Invariant 2 — a lone `"*"` matches every value, including
        /// the empty string. Operators rely on this as the
        /// allow-everything escape hatch.
        #[test]
        fn match_rule_glob_star_matches_everything(v in arb_exact_value()) {
            proptest::prop_assert!(
                match_rule_glob("*", &v),
                "pattern `*` must match any value; failed on {v:?}"
            );
        }

        /// Invariant 3 — N consecutive `?`s match exactly N-character
        /// values and reject any other length. Catches off-by-one in
        /// the loop at events.rs:1132-1163.
        #[test]
        fn match_rule_glob_all_questions_is_length_preserving(
            n in 1usize..=16,
            v in arb_exact_value(),
        ) {
            let pat: String = "?".repeat(n);
            let char_count = v.chars().count();
            let expected = char_count == n;
            proptest::prop_assert_eq!(
                match_rule_glob(&pat, &v),
                expected,
                "pattern `{}` must match iff value has {} chars (got {} chars in {:?})",
                pat, n, char_count, v
            );
        }

        /// Invariant 4 — empty pattern matches ONLY the empty value.
        /// Pins events.rs:1168-1169 which resolves `p_rem.is_empty()`
        /// against `v_rem` being fully consumed.
        #[test]
        fn match_rule_glob_empty_pattern_matches_only_empty_value(v in arb_exact_value()) {
            let expected = v.is_empty();
            proptest::prop_assert_eq!(
                match_rule_glob("", &v),
                expected,
                "empty pattern matched non-empty value {:?}",
                v
            );
        }

        /// Invariant 5 — a value can be matched by its own value used
        /// as a glob pattern when that value has no `*` or `?`. Same
        /// as Invariant 1 but expressed from the value's perspective
        /// (catches a bug that accepts reflexivity for literal chars
        /// but breaks when `.` / `:` / `_` are present).
        #[test]
        fn match_rule_glob_reflexive_on_wildcard_free_values(
            v in arb_exact_value(),
        ) {
            proptest::prop_assume!(!v.contains('*') && !v.contains('?'));
            proptest::prop_assert!(
                match_rule_glob(&v, &v),
                "reflexivity failed: match_rule_glob({v:?}, {v:?}) returned false"
            );
        }
    }
}
