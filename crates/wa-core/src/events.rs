//! Event bus for detections and signals
//!
//! Provides bounded broadcast channels and fanout for system events.
//!
//! # Architecture
//!
//! The event bus uses tokio's broadcast channels for multi-consumer fanout:
//! - Single producer publishes events via `EventBus::publish()`
//! - Subscribers can listen to all events or specific channels (delta/detection/signal)
//! - Bounded capacity provides backpressure (slow consumers get lagged)
//!
//! # Example
//!
//! ```no_run
//! use wa_core::events::{EventBus, Event};
//!
//! #[tokio::main]
//! async fn main() {
//!     let bus = EventBus::new(1000);
//!     let mut subscriber = bus.subscribe();
//!
//!     // Publish events
//!     bus.publish(Event::PaneDiscovered {
//!         pane_id: 1,
//!         domain: "local".to_string(),
//!         title: "shell".to_string(),
//!     });
//!
//!     // Receive events
//!     while let Ok(event) = subscriber.recv().await {
//!         println!("Got event: {:?}", event);
//!     }
//! }
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::patterns::Detection;
use crate::policy::Redactor;

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
    GapDetected { pane_id: u64, reason: String },

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
        /// Variable name (e.g., "WA_EVENT")
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
    for (key, value) in obj {
        let mut rendered = match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => "null".to_string(),
            _ => serde_json::to_string(value).unwrap_or_default(),
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
    let boundary = value
        .char_indices()
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx <= max_len)
        .last()
        .unwrap_or(0);
    value.truncate(boundary);
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
    /// Number of currently active subscribers
    pub active_subscribers: AtomicU64,
    /// Total lag events (slow consumer missed messages)
    pub subscriber_lag_events: AtomicU64,
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
            active_subscribers: self.active_subscribers.load(Ordering::Relaxed),
            subscriber_lag_events: self.subscriber_lag_events.load(Ordering::Relaxed),
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
    /// Current active subscriber count
    pub active_subscribers: u64,
    /// Total lag events across all subscribers
    pub subscriber_lag_events: u64,
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
}

/// Event bus for distributing events to subscribers via broadcast fanout
///
/// Uses tokio broadcast channel for multi-consumer delivery. The bus is
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
        self.all_sender.receiver_count()
            + self.delta_sender.receiver_count()
            + self.detection_sender.receiver_count()
            + self.signal_sender.receiver_count()
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
        self.metrics
            .events_published
            .fetch_add(1, Ordering::Relaxed);
        let mut delivered = 0usize;

        if let Ok(count) = self.all_sender.send(event.clone()) {
            delivered += count;
        }

        delivered += match event {
            Event::SegmentCaptured { .. } | Event::GapDetected { .. } => {
                self.send_routed(event, &self.delta_sender, &self.delta_times)
            }
            Event::PatternDetected { .. } => {
                self.send_routed(event, &self.detection_sender, &self.detection_times)
            }
            Event::PaneDiscovered { .. }
            | Event::PaneDisappeared { .. }
            | Event::WorkflowStarted { .. }
            | Event::WorkflowStep { .. }
            | Event::WorkflowCompleted { .. }
            | Event::UserVarReceived { .. } => {
                self.send_routed(event, &self.signal_sender, &self.signal_times)
            }
        };

        if delivered == 0 {
            self.metrics
                .events_dropped_no_subscribers
                .fetch_add(1, Ordering::Relaxed);
        }

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
        }
    }

    /// Snapshot queue depths and oldest message lag per channel
    #[must_use]
    pub fn stats(&self) -> EventBusStats {
        EventBusStats {
            capacity: self.capacity,
            delta_queued: self.delta_sender.len(),
            detection_queued: self.detection_sender.len(),
            signal_queued: self.signal_sender.len(),
            delta_subscribers: self.delta_sender.receiver_count(),
            detection_subscribers: self.detection_sender.receiver_count(),
            signal_subscribers: self.signal_sender.receiver_count(),
            delta_oldest_lag_ms: Self::oldest_lag_ms(&self.delta_times),
            detection_oldest_lag_ms: Self::oldest_lag_ms(&self.detection_times),
            signal_oldest_lag_ms: Self::oldest_lag_ms(&self.signal_times),
        }
    }

    fn send_routed(
        &self,
        event: Event,
        sender: &broadcast::Sender<Event>,
        times: &Mutex<VecDeque<Instant>>,
    ) -> usize {
        sender.send(event).map_or(0, |count| {
            Self::record_timestamp(times, self.capacity);
            count
        })
    }

    fn record_timestamp(times: &Mutex<VecDeque<Instant>>, capacity: usize) {
        if let Ok(mut guard) = times.lock() {
            guard.push_back(Instant::now());
            if guard.len() > capacity {
                guard.pop_front();
            }
        }
    }

    fn oldest_lag_ms(times: &Mutex<VecDeque<Instant>>) -> Option<u64> {
        let oldest = times.lock().ok()?.front().copied()?;
        let elapsed_ms = oldest.elapsed().as_millis();
        u64::try_from(elapsed_ms).ok()
    }
}

/// Error returned when receiving events
#[derive(Debug, Clone)]
pub enum RecvError {
    /// The event bus was closed (all senders dropped)
    Closed,
    /// Subscriber fell behind and missed events
    Lagged { missed_count: u64 },
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "event bus closed"),
            Self::Lagged { missed_count } => {
                write!(f, "subscriber lagged, missed {missed_count} events")
            }
        }
    }
}

impl std::error::Error for RecvError {}

/// Subscriber handle for receiving events from the bus
///
/// Dropping the subscriber automatically unsubscribes and decrements metrics.
pub struct EventSubscriber {
    receiver: broadcast::Receiver<Event>,
    metrics: Arc<EventBusMetrics>,
    lagged_count: u64,
}

impl EventSubscriber {
    /// Receive the next event
    ///
    /// Blocks until an event is available or the bus is closed.
    ///
    /// # Errors
    /// - `RecvError::Closed` if the event bus was dropped
    /// - `RecvError::Lagged` if this subscriber fell behind (events were missed)
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        match self.receiver.recv().await {
            Ok(event) => Ok(event),
            Err(broadcast::error::RecvError::Closed) => Err(RecvError::Closed),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // Track lag and return an error so caller knows they missed events
                self.lagged_count += n;
                self.metrics
                    .subscriber_lag_events
                    .fetch_add(n, Ordering::Relaxed);
                Err(RecvError::Lagged { missed_count: n })
            }
        }
    }

    /// Try to receive an event without blocking
    ///
    /// Returns `None` if no event is immediately available.
    pub fn try_recv(&mut self) -> Option<Result<Event, RecvError>> {
        match self.receiver.try_recv() {
            Ok(event) => Some(Ok(event)),
            Err(broadcast::error::TryRecvError::Empty) => None,
            Err(broadcast::error::TryRecvError::Closed) => Some(Err(RecvError::Closed)),
            Err(broadcast::error::TryRecvError::Lagged(n)) => {
                self.lagged_count += n;
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
            // Window expired: reset as new occurrence
            entry.count = 1;
            entry.first_seen = now;
            entry.last_seen = now;
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

    // Convert glob → regex
    let mut re = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | ':' => {
                re.push('\\');
                re.push(ch);
            }
            _ => re.push(ch),
        }
    }
    re.push('$');

    fancy_regex::Regex::new(&re).is_ok_and(|r| r.is_match(value).unwrap_or(false))
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
/// ```ignore
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
            reason: "test".to_string(),
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

    #[tokio::test]
    async fn publish_with_no_subscribers_counts_drops() {
        let bus = EventBus::new(10);

        let count = bus.publish(Event::PaneDisappeared { pane_id: 1 });
        assert_eq!(count, 0);

        let metrics = bus.metrics().snapshot();
        assert_eq!(metrics.events_published, 1);
        assert_eq!(metrics.events_dropped_no_subscribers, 1);
    }

    #[tokio::test]
    async fn subscriber_receives_published_events() {
        let bus = EventBus::new(10);
        let mut sub = bus.subscribe();

        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "shell".to_string(),
        });

        let event = sub.recv().await.unwrap();
        assert!(matches!(event, Event::PaneDiscovered { pane_id: 1, .. }));
    }

    #[tokio::test]
    async fn multiple_subscribers_fanout() {
        let bus = EventBus::new(10);
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();

        assert_eq!(bus.subscriber_count(), 2);

        let _ = bus.publish(Event::PaneDisappeared { pane_id: 42 });

        let e1 = sub1.recv().await.unwrap();
        let e2 = sub2.recv().await.unwrap();

        assert!(matches!(e1, Event::PaneDisappeared { pane_id: 42 }));
        assert!(matches!(e2, Event::PaneDisappeared { pane_id: 42 }));
    }

    #[tokio::test]
    async fn delta_subscriber_only_sees_delta_events() {
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
    }

    #[tokio::test]
    async fn detection_subscriber_receives_pattern_events() {
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
    }

    #[tokio::test]
    async fn subscriber_drop_decrements_count() {
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
    }

    #[tokio::test]
    async fn try_recv_returns_none_when_empty() {
        let bus = EventBus::new(10);
        let mut sub = bus.subscribe();

        assert!(sub.try_recv().is_none());
    }

    #[tokio::test]
    async fn try_recv_returns_event_when_available() {
        let bus = EventBus::new(10);
        let mut sub = bus.subscribe();

        let _ = bus.publish(Event::PaneDisappeared { pane_id: 1 });

        let result = sub.try_recv();
        assert!(result.is_some());
        assert!(matches!(
            result.unwrap().unwrap(),
            Event::PaneDisappeared { pane_id: 1 }
        ));
    }

    #[tokio::test]
    async fn backpressure_causes_lag() {
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
            Err(RecvError::Closed) => panic!("unexpected close"),
        }

        // Lag should be tracked in metrics
        let metrics = bus.metrics().snapshot();
        assert!(metrics.subscriber_lag_events > 0 || sub.lagged_count() > 0);
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
    fn metrics_snapshot_is_serializable() {
        let metrics = MetricsSnapshot {
            events_published: 100,
            events_dropped_no_subscribers: 5,
            active_subscribers: 3,
            subscriber_lag_events: 10,
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

    #[tokio::test]
    async fn recv_error_display() {
        let err = RecvError::Closed;
        assert_eq!(format!("{err}"), "event bus closed");

        let err = RecvError::Lagged { missed_count: 42 };
        assert_eq!(format!("{err}"), "subscriber lagged, missed 42 events");
    }

    #[tokio::test]
    async fn uptime_increases() {
        let bus = EventBus::new(10);
        let t1 = bus.uptime();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let t2 = bus.uptime();
        assert!(t2 > t1);
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
            name: "WA_EVENT".to_string(),
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
        assert!(!match_rule_glob("codex.error", "codex.warning"));
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
    fn severity_level_ordering() {
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
        assert_eq!(parse_severity("bogus"), None);
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
    #[tokio::test]
    async fn e2e_mute_lifecycle_with_storage() {
        use crate::storage::{EventMuteRecord, StorageHandle};

        let db_path = std::env::temp_dir().join(format!("wa_e2e_mute_{}.db", std::process::id()));
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
    }

    /// E2E: mute with expiry automatically expires.
    #[tokio::test]
    async fn e2e_mute_expiry() {
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
    }

    /// E2E: full noise control pipeline - dedup → cooldown → mute check.
    #[tokio::test]
    async fn e2e_full_pipeline_dedup_cooldown_mute() {
        use crate::storage::{EventMuteRecord, StorageHandle};

        let db_path =
            std::env::temp_dir().join(format!("wa_e2e_full_pipeline_{}.db", std::process::id()));
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
}
