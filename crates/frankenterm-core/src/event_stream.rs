//! Streaming event subscriptions with filtering, cursors, and wait primitives.
//!
//! Provides deterministic, resumable event streams for autonomous agent
//! coordination. Agents can subscribe to filtered event streams with cursor
//! checkpoints, replay from storage, and condition-based wait primitives.
//!
//! # Architecture
//!
//! The streaming layer bridges two event sources:
//! 1. **Storage** — Historical events persisted in SQLite (cursor-based replay)
//! 2. **EventBus** — Live events from the broadcast bus (real-time)
//!
//! A `FilteredEventStream` first drains historical events from storage
//! (starting from a cursor), then seamlessly transitions to live bus events.
//! This ensures no events are missed during the handoff.
//!
//! # Example
//!
//! ```no_run
//! use frankenterm_core::event_stream::{EventStreamFilter, StreamCursor, EventWaiter, WaitCondition};
//!
//! // Create a filter for detection events on pane 0
//! let filter = EventStreamFilter::builder()
//!     .pane_id(0)
//!     .event_types(vec!["pattern_detected".to_string()])
//!     .build();
//!
//! // Resume from a previous cursor
//! let cursor = StreamCursor::after_id(42);
//!
//! // Wait for a specific pattern with timeout
//! let condition = WaitCondition::rule_id("codex.usage.reached");
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::events::{Event, EventBus, EventSubscriber};
use crate::patterns::Detection;

// =============================================================================
// Stream Cursor
// =============================================================================

/// Cursor for resumable event streaming.
///
/// Cursors use the monotonic storage event ID as the position marker.
/// After disconnection, clients resume by providing their last seen cursor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamCursor {
    /// Last seen event ID (exclusive — stream resumes *after* this ID)
    pub after_id: i64,
    /// Optional correlation ID for grouping related streams
    pub correlation_id: Option<String>,
}

impl StreamCursor {
    /// Create a cursor that starts after the given event ID.
    #[must_use]
    pub fn after_id(id: i64) -> Self {
        Self {
            after_id: id,
            correlation_id: None,
        }
    }

    /// Create a cursor starting from the beginning (all events).
    #[must_use]
    pub fn from_beginning() -> Self {
        Self {
            after_id: 0,
            correlation_id: None,
        }
    }

    /// Attach a correlation ID for tracking related streams.
    #[must_use]
    pub fn with_correlation_id(mut self, id: String) -> Self {
        self.correlation_id = Some(id);
        self
    }

    /// Advance the cursor to a new position.
    pub fn advance(&mut self, new_id: i64) {
        if new_id > self.after_id {
            self.after_id = new_id;
        }
    }
}

impl Default for StreamCursor {
    fn default() -> Self {
        Self::from_beginning()
    }
}

// =============================================================================
// Event Stream Filter
// =============================================================================

/// Predicate-based filter for event streams.
///
/// Filters are applied both to storage queries (push-down) and to live bus
/// events (in-memory). An event must match ALL specified criteria (AND logic).
/// Empty filter fields match everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventStreamFilter {
    /// Filter to events from these pane IDs (empty = all panes)
    pub pane_ids: Vec<u64>,
    /// Filter to events matching these rule IDs (empty = all rules)
    pub rule_ids: Vec<String>,
    /// Filter to these event type names (empty = all types)
    pub event_types: Vec<String>,
    /// Minimum severity (None = all severities)
    pub min_severity: Option<SeverityLevel>,
    /// Only unhandled events
    pub unhandled_only: bool,
    /// Time range: events after this epoch ms (inclusive)
    pub since_ms: Option<i64>,
    /// Time range: events before this epoch ms (exclusive)
    pub until_ms: Option<i64>,
}

/// Severity levels for filtering, ordered from lowest to highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeverityLevel {
    /// Informational
    Info,
    /// Warning
    Warning,
    /// Critical
    Critical,
}

impl SeverityLevel {
    /// Parse from string (case-insensitive).
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "info" | "informational" => Some(Self::Info),
            "warning" | "warn" => Some(Self::Warning),
            "critical" | "crit" | "error" => Some(Self::Critical),
            _ => None,
        }
    }
}

impl EventStreamFilter {
    /// Create a new builder for constructing filters.
    #[must_use]
    pub fn builder() -> EventStreamFilterBuilder {
        EventStreamFilterBuilder::default()
    }

    /// Returns true if this filter accepts the given event.
    #[must_use]
    pub fn matches_event(&self, event: &Event) -> bool {
        // Pane ID filter
        if !self.pane_ids.is_empty() {
            match event.pane_id() {
                Some(pid) => {
                    if !self.pane_ids.contains(&pid) {
                        return false;
                    }
                }
                None => return false, // No pane_id and filter requires one
            }
        }

        // Event type filter
        if !self.event_types.is_empty() && !self.event_types.iter().any(|t| t == event.type_name())
        {
            return false;
        }

        // Rule ID filter (only applies to PatternDetected)
        if !self.rule_ids.is_empty() {
            if let Event::PatternDetected { detection, .. } = event {
                if !self.rule_ids.contains(&detection.rule_id) {
                    return false;
                }
            } else if self.event_types.is_empty() {
                // If no event_type filter but rule_ids specified, only match detections
                return false;
            }
        }

        // Handled state only exists for detection events. On the live bus,
        // every detection is initially unhandled until a workflow records it.
        if self.unhandled_only && !matches!(event, Event::PatternDetected { .. }) {
            return false;
        }

        // Severity filter (only applies to PatternDetected)
        if let Some(min_sev) = self.min_severity {
            if let Event::PatternDetected { detection, .. } = event {
                let event_sev = severity_from_detection(detection);
                if event_sev < min_sev {
                    return false;
                }
            }
            // Non-detection events pass severity filter (they don't have severity)
        }

        true
    }

    /// Returns true if this filter has no active predicates (matches everything).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pane_ids.is_empty()
            && self.rule_ids.is_empty()
            && self.event_types.is_empty()
            && self.min_severity.is_none()
            && !self.unhandled_only
            && self.since_ms.is_none()
            && self.until_ms.is_none()
    }
}

fn severity_from_detection(detection: &Detection) -> SeverityLevel {
    match detection.severity {
        crate::patterns::Severity::Info => SeverityLevel::Info,
        crate::patterns::Severity::Warning => SeverityLevel::Warning,
        crate::patterns::Severity::Critical => SeverityLevel::Critical,
    }
}

/// Builder for `EventStreamFilter`.
#[derive(Debug, Default)]
pub struct EventStreamFilterBuilder {
    filter: EventStreamFilter,
}

impl EventStreamFilterBuilder {
    /// Filter to a single pane.
    #[must_use]
    pub fn pane_id(mut self, id: u64) -> Self {
        self.filter.pane_ids.push(id);
        self
    }

    /// Filter to multiple panes.
    #[must_use]
    pub fn pane_ids(mut self, ids: Vec<u64>) -> Self {
        self.filter.pane_ids = ids;
        self
    }

    /// Filter to a single rule ID.
    #[must_use]
    pub fn rule_id(mut self, id: String) -> Self {
        self.filter.rule_ids.push(id);
        self
    }

    /// Filter to multiple rule IDs.
    #[must_use]
    pub fn rule_ids(mut self, ids: Vec<String>) -> Self {
        self.filter.rule_ids = ids;
        self
    }

    /// Filter to specific event types.
    #[must_use]
    pub fn event_types(mut self, types: Vec<String>) -> Self {
        self.filter.event_types = types;
        self
    }

    /// Set minimum severity.
    #[must_use]
    pub fn min_severity(mut self, sev: SeverityLevel) -> Self {
        self.filter.min_severity = Some(sev);
        self
    }

    /// Only include unhandled events.
    #[must_use]
    pub fn unhandled_only(mut self) -> Self {
        self.filter.unhandled_only = true;
        self
    }

    /// Events after this timestamp (epoch ms, inclusive).
    #[must_use]
    pub fn since_ms(mut self, ms: i64) -> Self {
        self.filter.since_ms = Some(ms);
        self
    }

    /// Events before this timestamp (epoch ms, exclusive).
    #[must_use]
    pub fn until_ms(mut self, ms: i64) -> Self {
        self.filter.until_ms = Some(ms);
        self
    }

    /// Build the filter.
    #[must_use]
    pub fn build(self) -> EventStreamFilter {
        self.filter
    }
}

// =============================================================================
// Wait Conditions
// =============================================================================

/// Condition that must be satisfied for a wait to complete.
///
/// Wait conditions are evaluated against incoming events. When a matching
/// event is received, the wait resolves with that event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait for any event matching the filter.
    AnyEvent,

    /// Wait for a specific rule ID to fire.
    RuleId {
        /// The rule ID to match (e.g., "codex.usage.reached")
        rule_id: String,
    },

    /// Wait for any pattern detection on a pane.
    PaneDetection {
        /// The pane to watch
        pane_id: u64,
    },

    /// Wait for a pane to appear.
    PaneDiscovered {
        /// Optional: specific pane ID (None = any pane)
        pane_id: Option<u64>,
    },

    /// Wait for a pane to disappear.
    PaneDisappeared {
        /// The pane ID to watch
        pane_id: u64,
    },

    /// Wait for a workflow to complete.
    WorkflowCompleted {
        /// Optional: specific workflow ID
        workflow_id: Option<String>,
    },

    /// Wait for any of several conditions (OR logic).
    AnyOf {
        /// Conditions to check (first match wins)
        conditions: Vec<WaitCondition>,
    },

    /// Wait for all conditions to be met (AND logic, across events).
    AllOf {
        /// Conditions that must all be satisfied
        conditions: Vec<WaitCondition>,
    },
}

impl WaitCondition {
    /// Convenience: wait for a specific rule ID.
    #[must_use]
    pub fn rule_id(id: &str) -> Self {
        Self::RuleId {
            rule_id: id.to_string(),
        }
    }

    /// Convenience: wait for any detection on a pane.
    #[must_use]
    pub fn pane_detection(pane_id: u64) -> Self {
        Self::PaneDetection { pane_id }
    }

    /// Convenience: wait for a pane to appear.
    #[must_use]
    pub fn pane_discovered(pane_id: Option<u64>) -> Self {
        Self::PaneDiscovered { pane_id }
    }

    /// Convenience: wait for a workflow to finish.
    #[must_use]
    pub fn workflow_completed(workflow_id: Option<String>) -> Self {
        Self::WorkflowCompleted { workflow_id }
    }

    /// Evaluate whether an event satisfies this condition.
    #[must_use]
    pub fn matches(&self, event: &Event) -> bool {
        match self {
            Self::AnyEvent => true,

            Self::RuleId { rule_id } => matches!(
                event,
                Event::PatternDetected { detection, .. } if detection.rule_id == *rule_id
            ),

            Self::PaneDetection { pane_id } => matches!(
                event,
                Event::PatternDetected { pane_id: pid, .. } if *pid == *pane_id
            ),

            Self::PaneDiscovered { pane_id } => matches!(
                event,
                Event::PaneDiscovered { pane_id: pid, .. }
                    if pane_id.is_none() || pane_id == &Some(*pid)
            ),

            Self::PaneDisappeared { pane_id } => matches!(
                event,
                Event::PaneDisappeared { pane_id: pid } if *pid == *pane_id
            ),

            Self::WorkflowCompleted { workflow_id } => matches!(
                event,
                Event::WorkflowCompleted { workflow_id: wid, .. }
                    if workflow_id.is_none() || workflow_id.as_deref() == Some(wid.as_str())
            ),

            Self::AnyOf { conditions } => conditions.iter().any(|c| c.matches(event)),

            Self::AllOf { .. } => {
                // AllOf requires state tracking across events — single-event
                // match is not sufficient. Return false here; use AllOfTracker
                // for stateful evaluation.
                false
            }
        }
    }
}

// =============================================================================
// Wait Result
// =============================================================================

/// Outcome of a wait operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WaitResult {
    /// Condition was satisfied by a matching event.
    Matched {
        /// The event that satisfied the condition.
        event: Event,
        /// How long the wait took (ms).
        elapsed_ms: u64,
    },
    /// Wait timed out before condition was met.
    Timeout {
        /// How long we waited (ms).
        elapsed_ms: u64,
    },
    /// Wait was cancelled.
    Cancelled {
        /// Reason for cancellation.
        reason: String,
    },
}

impl WaitResult {
    /// Returns true if the condition was satisfied.
    #[must_use]
    pub fn is_matched(&self) -> bool {
        matches!(self, Self::Matched { .. })
    }

    /// Returns true if the wait timed out.
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Timeout { .. })
    }
}

// =============================================================================
// EventWaiter — Condition-based wait primitive
// =============================================================================

/// Wait primitive for blocking until an event condition is met.
///
/// The waiter subscribes to the event bus and evaluates incoming events
/// against the specified condition. When a match is found (or timeout
/// expires), the result is returned.
pub struct EventWaiter {
    /// The condition to wait for
    condition: WaitCondition,
    /// Optional filter to pre-screen events
    filter: EventStreamFilter,
    /// Maximum wait duration
    timeout: Duration,
}

impl EventWaiter {
    /// Create a new waiter for the given condition.
    #[must_use]
    pub fn new(condition: WaitCondition) -> Self {
        Self {
            condition,
            filter: EventStreamFilter::default(),
            timeout: Duration::from_secs(3600), // Default 1 hour
        }
    }

    /// Set the maximum wait duration.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set an event filter to pre-screen events before condition evaluation.
    #[must_use]
    pub fn with_filter(mut self, filter: EventStreamFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Execute the wait against an event bus.
    ///
    /// Subscribes to the bus and blocks until the condition is met or
    /// timeout expires. Returns the matching event or timeout result.
    pub async fn wait(self, bus: &EventBus) -> WaitResult {
        let Self {
            condition,
            filter,
            timeout,
        } = self;
        let start = std::time::Instant::now();
        let mut subscriber = bus.subscribe();

        let recv_loop = async move {
            match condition {
                WaitCondition::AllOf { conditions } => {
                    let mut tracker = AllOfTracker::new(conditions);
                    loop {
                        match subscriber.recv().await {
                            Ok(event) => {
                                if filter.matches_event(&event) && tracker.check(&event) {
                                    return event;
                                }
                            }
                            Err(_) => {
                                // Channel closed or lagged — continue
                            }
                        }
                    }
                }
                condition => loop {
                    match subscriber.recv().await {
                        Ok(event) => {
                            if filter.matches_event(&event) && condition.matches(&event) {
                                return event;
                            }
                        }
                        Err(_) => {
                            // Channel closed or lagged — continue
                        }
                    }
                },
            };
        };

        match crate::runtime_compat::timeout(timeout, recv_loop).await {
            Ok(event) => WaitResult::Matched {
                event,
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
            Err(_) => WaitResult::Timeout {
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
        }
    }
}

// =============================================================================
// AllOfTracker — Stateful AND condition evaluation
// =============================================================================

/// Tracks progress toward satisfying all conditions in an AllOf wait.
///
/// Each sub-condition is tracked independently. When all have been
/// satisfied (possibly by different events), the tracker reports complete.
pub struct AllOfTracker {
    /// Conditions that still need to be met.
    remaining: Vec<WaitCondition>,
    /// Events that satisfied each condition.
    matched_events: Vec<Event>,
}

impl AllOfTracker {
    /// Create a new tracker for the given conditions.
    #[must_use]
    pub fn new(conditions: Vec<WaitCondition>) -> Self {
        Self {
            remaining: conditions,
            matched_events: Vec::new(),
        }
    }

    /// Check if an event satisfies any remaining condition.
    ///
    /// Returns true when all conditions have been satisfied.
    pub fn check(&mut self, event: &Event) -> bool {
        if let Some(idx) = self.remaining.iter().position(|c| c.matches(event)) {
            self.remaining.remove(idx);
            self.matched_events.push(event.clone());
        }
        self.remaining.is_empty()
    }

    /// Returns true if all conditions are satisfied.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.remaining.is_empty()
    }

    /// Number of conditions remaining.
    #[must_use]
    pub fn remaining_count(&self) -> usize {
        self.remaining.len()
    }

    /// Events that have been matched so far.
    #[must_use]
    pub fn matched_events(&self) -> &[Event] {
        &self.matched_events
    }
}

// =============================================================================
// FilteredEventStream — Cursor-based streaming with filter push-down
// =============================================================================

/// A filtered, cursor-based event stream.
///
/// Combines storage replay (for historical events) with live bus subscription
/// (for new events), applying the same filter to both sources.
///
/// # Ordering Guarantees
///
/// - Historical events are ordered by ascending storage ID (deterministic).
/// - Live events arrive in publish order (broadcast channel FIFO).
/// - The transition from historical to live is seamless: the cursor tracks
///   the last historical event ID, and the bus subscription starts before
///   the storage query completes to avoid gaps.
pub struct FilteredEventStream {
    /// Filter applied to all events.
    filter: EventStreamFilter,
    /// Current cursor position.
    cursor: StreamCursor,
    /// Live event subscriber.
    subscriber: EventSubscriber,
    /// Maximum events per batch (for bounded replay).
    batch_size: usize,
    /// Telemetry: total events delivered.
    delivered: Arc<AtomicU64>,
    /// Telemetry: total events filtered out.
    filtered_out: Arc<AtomicU64>,
}

impl FilteredEventStream {
    /// Create a new filtered stream.
    ///
    /// # Arguments
    /// * `bus` - The event bus to subscribe to for live events
    /// * `filter` - Predicate filter for events
    /// * `cursor` - Starting position (use `StreamCursor::from_beginning()` for all)
    /// * `batch_size` - Max events per batch when replaying from storage
    #[must_use]
    pub fn new(
        bus: &EventBus,
        filter: EventStreamFilter,
        cursor: StreamCursor,
        batch_size: usize,
    ) -> Self {
        Self {
            filter,
            cursor,
            subscriber: bus.subscribe(),
            batch_size: batch_size.max(1),
            delivered: Arc::new(AtomicU64::new(0)),
            filtered_out: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Receive the next event that matches the filter.
    ///
    /// Blocks until a matching event arrives or the channel closes.
    /// Updates the cursor on each delivered event (if it has a storage ID).
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            match self.subscriber.recv().await {
                Ok(event) => {
                    if self.filter.matches_event(&event) {
                        self.delivered.fetch_add(1, Ordering::Relaxed);
                        // Advance cursor if this is a persisted detection with an ID
                        if let Event::PatternDetected {
                            event_id: Some(id), ..
                        } = &event
                        {
                            self.cursor.advance(*id);
                        }
                        return Some(event);
                    }
                    self.filtered_out.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    // Channel closed or lagged — try to recover
                }
            }
        }
    }

    /// Try to receive the next matching event without blocking.
    ///
    /// Returns `None` if no matching event is immediately available.
    pub fn try_next(&mut self) -> Option<Event> {
        loop {
            match self.subscriber.try_recv() {
                Some(Ok(event)) => {
                    if self.filter.matches_event(&event) {
                        self.delivered.fetch_add(1, Ordering::Relaxed);
                        if let Event::PatternDetected {
                            event_id: Some(id), ..
                        } = &event
                        {
                            self.cursor.advance(*id);
                        }
                        return Some(event);
                    }
                    self.filtered_out.fetch_add(1, Ordering::Relaxed);
                }
                Some(Err(_)) | None => return None,
            }
        }
    }

    /// Get the current cursor position.
    #[must_use]
    pub fn cursor(&self) -> &StreamCursor {
        &self.cursor
    }

    /// Get the filter.
    #[must_use]
    pub fn filter(&self) -> &EventStreamFilter {
        &self.filter
    }

    /// Get the batch size for storage replay.
    #[must_use]
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Get stream telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> StreamTelemetry {
        StreamTelemetry {
            delivered: self.delivered.load(Ordering::Relaxed),
            filtered_out: self.filtered_out.load(Ordering::Relaxed),
            cursor_position: self.cursor.after_id,
            correlation_id: self.cursor.correlation_id.clone(),
        }
    }
}

/// Telemetry snapshot for a filtered event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamTelemetry {
    /// Total events delivered through the stream.
    pub delivered: u64,
    /// Total events filtered out.
    pub filtered_out: u64,
    /// Current cursor position.
    pub cursor_position: i64,
    /// Correlation ID, if set.
    pub correlation_id: Option<String>,
}

// =============================================================================
// Subscription Registry — Tracks active subscriptions
// =============================================================================

/// Unique subscription identifier.
pub type SubscriptionId = u64;

/// Metadata about an active subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionInfo {
    /// Unique subscription ID.
    pub id: SubscriptionId,
    /// Filter applied to this subscription.
    pub filter: EventStreamFilter,
    /// Current cursor position.
    pub cursor: StreamCursor,
    /// When the subscription was created (epoch ms).
    pub created_at_ms: u64,
    /// Total events delivered.
    pub events_delivered: u64,
    /// Total events filtered out.
    pub events_filtered_out: u64,
}

/// Registry of active streaming subscriptions.
///
/// Tracks all active subscriptions for introspection and management.
/// Used by the robot API to expose subscription state.
pub struct SubscriptionRegistry {
    next_id: AtomicU64,
    subscriptions: std::sync::Mutex<std::collections::HashMap<SubscriptionId, SubscriptionInfo>>,
}

impl Default for SubscriptionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionRegistry {
    /// Create a new registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            subscriptions: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Register a new subscription and return its ID.
    pub fn register(
        &self,
        filter: EventStreamFilter,
        cursor: StreamCursor,
        now_ms: u64,
    ) -> SubscriptionId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let info = SubscriptionInfo {
            id,
            filter,
            cursor,
            created_at_ms: now_ms,
            events_delivered: 0,
            events_filtered_out: 0,
        };
        if let Ok(mut subs) = self.subscriptions.lock() {
            subs.insert(id, info);
        }
        id
    }

    /// Remove a subscription.
    pub fn unregister(&self, id: SubscriptionId) -> Option<SubscriptionInfo> {
        self.subscriptions
            .lock()
            .ok()
            .and_then(|mut subs| subs.remove(&id))
    }

    /// Update telemetry for a subscription.
    pub fn update_telemetry(&self, id: SubscriptionId, telemetry: &StreamTelemetry) {
        if let Ok(mut subs) = self.subscriptions.lock() {
            if let Some(info) = subs.get_mut(&id) {
                info.events_delivered = telemetry.delivered;
                info.events_filtered_out = telemetry.filtered_out;
                info.cursor.after_id = telemetry.cursor_position;
            }
        }
    }

    /// List all active subscriptions.
    #[must_use]
    pub fn list(&self) -> Vec<SubscriptionInfo> {
        self.subscriptions
            .lock()
            .map(|subs| subs.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Get a specific subscription.
    #[must_use]
    pub fn get(&self, id: SubscriptionId) -> Option<SubscriptionInfo> {
        self.subscriptions
            .lock()
            .ok()
            .and_then(|subs| subs.get(&id).cloned())
    }

    /// Number of active subscriptions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.subscriptions
            .lock()
            .map(|subs| subs.len())
            .unwrap_or(0)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{Detection, Severity};
    use std::sync::Arc;

    fn make_detection(rule_id: &str, severity: Severity) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type: crate::patterns::AgentType::Unknown,
            event_type: "test".to_string(),
            severity,
            confidence: 1.0,
            extracted: serde_json::Value::Object(serde_json::Map::new()),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    fn make_pattern_event(pane_id: u64, rule_id: &str, event_id: Option<i64>) -> Event {
        Event::PatternDetected {
            pane_id,
            pane_uuid: None,
            detection: make_detection(rule_id, Severity::Warning),
            event_id,
        }
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;

        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build compat runtime for test");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // --- StreamCursor tests ---

    #[test]
    fn cursor_from_beginning_starts_at_zero() {
        let cursor = StreamCursor::from_beginning();
        assert_eq!(cursor.after_id, 0);
        assert!(cursor.correlation_id.is_none());
    }

    #[test]
    fn cursor_after_id_sets_position() {
        let cursor = StreamCursor::after_id(42);
        assert_eq!(cursor.after_id, 42);
    }

    #[test]
    fn cursor_advance_moves_forward() {
        let mut cursor = StreamCursor::after_id(10);
        cursor.advance(20);
        assert_eq!(cursor.after_id, 20);
    }

    #[test]
    fn cursor_advance_ignores_backwards() {
        let mut cursor = StreamCursor::after_id(20);
        cursor.advance(10);
        assert_eq!(cursor.after_id, 20);
    }

    #[test]
    fn cursor_with_correlation_id() {
        let cursor = StreamCursor::after_id(1).with_correlation_id("req-123".to_string());
        assert_eq!(cursor.correlation_id.as_deref(), Some("req-123"));
    }

    #[test]
    fn cursor_serde_roundtrip() {
        let cursor = StreamCursor::after_id(42).with_correlation_id("test".to_string());
        let json = serde_json::to_string(&cursor).unwrap();
        let parsed: StreamCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(cursor, parsed);
    }

    // --- EventStreamFilter tests ---

    #[test]
    fn empty_filter_matches_everything() {
        let filter = EventStreamFilter::default();
        assert!(filter.is_empty());

        let event = Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "test".to_string(),
        };
        assert!(filter.matches_event(&event));
    }

    #[test]
    fn pane_id_filter() {
        let filter = EventStreamFilter::builder().pane_id(1).build();
        assert!(filter.matches_event(&Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
        assert!(!filter.matches_event(&Event::PaneDiscovered {
            pane_id: 2,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
    }

    #[test]
    fn event_type_filter() {
        let filter = EventStreamFilter::builder()
            .event_types(vec!["pane_discovered".to_string()])
            .build();
        assert!(filter.matches_event(&Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
        assert!(!filter.matches_event(&Event::PaneDisappeared { pane_id: 1 }));
    }

    #[test]
    fn rule_id_filter() {
        let filter = EventStreamFilter::builder()
            .rule_id("codex.usage.reached".to_string())
            .build();
        assert!(filter.matches_event(&make_pattern_event(1, "codex.usage.reached", None)));
        assert!(!filter.matches_event(&make_pattern_event(1, "other.rule", None)));
    }

    #[test]
    fn severity_filter() {
        let filter = EventStreamFilter::builder()
            .min_severity(SeverityLevel::Warning)
            .build();

        let warning_event = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection: make_detection("test", Severity::Warning),
            event_id: None,
        };
        assert!(filter.matches_event(&warning_event));

        let info_event = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection: make_detection("test", Severity::Info),
            event_id: None,
        };
        assert!(!filter.matches_event(&info_event));

        let critical_event = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection: make_detection("test", Severity::Critical),
            event_id: None,
        };
        assert!(filter.matches_event(&critical_event));
    }

    #[test]
    fn combined_filter_is_and_logic() {
        let filter = EventStreamFilter::builder()
            .pane_id(1)
            .rule_id("test.rule".to_string())
            .build();

        // Matches both criteria
        assert!(filter.matches_event(&make_pattern_event(1, "test.rule", None)));
        // Wrong pane
        assert!(!filter.matches_event(&make_pattern_event(2, "test.rule", None)));
        // Wrong rule
        assert!(!filter.matches_event(&make_pattern_event(1, "other.rule", None)));
    }

    #[test]
    fn unhandled_only_filter_only_matches_live_detections() {
        let filter = EventStreamFilter::builder().unhandled_only().build();

        assert!(filter.matches_event(&make_pattern_event(1, "test.rule", None)));
        assert!(!filter.matches_event(&Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
    }

    #[test]
    fn filter_serde_roundtrip() {
        let filter = EventStreamFilter::builder()
            .pane_id(1)
            .rule_id("test".to_string())
            .min_severity(SeverityLevel::Warning)
            .unhandled_only()
            .since_ms(1000)
            .until_ms(2000)
            .build();
        let json = serde_json::to_string(&filter).unwrap();
        let parsed: EventStreamFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(filter.pane_ids, parsed.pane_ids);
        assert_eq!(filter.rule_ids, parsed.rule_ids);
        assert_eq!(filter.min_severity, parsed.min_severity);
    }

    // --- WaitCondition tests ---

    #[test]
    fn wait_any_event_matches_all() {
        let cond = WaitCondition::AnyEvent;
        assert!(cond.matches(&Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
    }

    #[test]
    fn wait_rule_id_matches_detection() {
        let cond = WaitCondition::rule_id("codex.usage.reached");
        assert!(cond.matches(&make_pattern_event(1, "codex.usage.reached", None)));
        assert!(!cond.matches(&make_pattern_event(1, "other.rule", None)));
    }

    #[test]
    fn wait_pane_detection_matches_pane() {
        let cond = WaitCondition::pane_detection(1);
        assert!(cond.matches(&make_pattern_event(1, "any.rule", None)));
        assert!(!cond.matches(&make_pattern_event(2, "any.rule", None)));
    }

    #[test]
    fn wait_pane_discovered_any() {
        let cond = WaitCondition::pane_discovered(None);
        assert!(cond.matches(&Event::PaneDiscovered {
            pane_id: 99,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
    }

    #[test]
    fn wait_pane_discovered_specific() {
        let cond = WaitCondition::pane_discovered(Some(5));
        assert!(cond.matches(&Event::PaneDiscovered {
            pane_id: 5,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
        assert!(!cond.matches(&Event::PaneDiscovered {
            pane_id: 6,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
    }

    #[test]
    fn wait_workflow_completed_any() {
        let cond = WaitCondition::workflow_completed(None);
        assert!(cond.matches(&Event::WorkflowCompleted {
            workflow_id: "wf-1".to_string(),
            success: true,
            reason: None,
        }));
    }

    #[test]
    fn wait_workflow_completed_specific() {
        let cond = WaitCondition::workflow_completed(Some("wf-1".to_string()));
        assert!(cond.matches(&Event::WorkflowCompleted {
            workflow_id: "wf-1".to_string(),
            success: true,
            reason: None,
        }));
        assert!(!cond.matches(&Event::WorkflowCompleted {
            workflow_id: "wf-2".to_string(),
            success: true,
            reason: None,
        }));
    }

    #[test]
    fn wait_any_of_matches_first() {
        let cond = WaitCondition::AnyOf {
            conditions: vec![WaitCondition::rule_id("a"), WaitCondition::rule_id("b")],
        };
        assert!(cond.matches(&make_pattern_event(1, "a", None)));
        assert!(cond.matches(&make_pattern_event(1, "b", None)));
        assert!(!cond.matches(&make_pattern_event(1, "c", None)));
    }

    #[test]
    fn wait_condition_serde_roundtrip() {
        let cond = WaitCondition::AnyOf {
            conditions: vec![
                WaitCondition::rule_id("test"),
                WaitCondition::pane_detection(1),
            ],
        };
        let json = serde_json::to_string(&cond).unwrap();
        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        // Verify it still matches correctly after roundtrip
        assert!(parsed.matches(&make_pattern_event(1, "test", None)));
    }

    // --- AllOfTracker tests ---

    #[test]
    fn all_of_tracker_requires_all_conditions() {
        let mut tracker = AllOfTracker::new(vec![
            WaitCondition::rule_id("a"),
            WaitCondition::rule_id("b"),
        ]);
        assert!(!tracker.is_complete());
        assert_eq!(tracker.remaining_count(), 2);

        // First condition met
        assert!(!tracker.check(&make_pattern_event(1, "a", None)));
        assert_eq!(tracker.remaining_count(), 1);

        // Second condition met
        assert!(tracker.check(&make_pattern_event(1, "b", None)));
        assert!(tracker.is_complete());
        assert_eq!(tracker.matched_events().len(), 2);
    }

    #[test]
    fn all_of_tracker_ignores_non_matching() {
        let mut tracker = AllOfTracker::new(vec![WaitCondition::rule_id("target")]);
        assert!(!tracker.check(&make_pattern_event(1, "other", None)));
        assert!(!tracker.is_complete());
    }

    // --- WaitResult tests ---

    #[test]
    fn wait_result_matched() {
        let result = WaitResult::Matched {
            event: Event::PaneDiscovered {
                pane_id: 1,
                domain: "local".to_string(),
                title: "test".to_string(),
            },
            elapsed_ms: 100,
        };
        assert!(result.is_matched());
        assert!(!result.is_timeout());
    }

    #[test]
    fn wait_result_timeout() {
        let result = WaitResult::Timeout { elapsed_ms: 5000 };
        assert!(!result.is_matched());
        assert!(result.is_timeout());
    }

    // --- SubscriptionRegistry tests ---

    #[test]
    fn registry_register_and_list() {
        let registry = SubscriptionRegistry::new();
        let filter = EventStreamFilter::builder().pane_id(1).build();
        let cursor = StreamCursor::from_beginning();

        let id = registry.register(filter, cursor, 1000);
        assert_eq!(registry.count(), 1);

        let subs = registry.list();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, id);
        assert_eq!(subs[0].created_at_ms, 1000);
    }

    #[test]
    fn registry_unregister() {
        let registry = SubscriptionRegistry::new();
        let id = registry.register(
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            1000,
        );
        assert_eq!(registry.count(), 1);

        let removed = registry.unregister(id);
        assert!(removed.is_some());
        assert_eq!(registry.count(), 0);
    }

    #[test]
    fn registry_update_telemetry() {
        let registry = SubscriptionRegistry::new();
        let id = registry.register(
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            1000,
        );

        let telemetry = StreamTelemetry {
            delivered: 42,
            filtered_out: 10,
            cursor_position: 100,
            correlation_id: None,
        };
        registry.update_telemetry(id, &telemetry);

        let info = registry.get(id).unwrap();
        assert_eq!(info.events_delivered, 42);
        assert_eq!(info.events_filtered_out, 10);
        assert_eq!(info.cursor.after_id, 100);
    }

    #[test]
    fn registry_ids_are_unique() {
        let registry = SubscriptionRegistry::new();
        let id1 = registry.register(
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            1000,
        );
        let id2 = registry.register(
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            1001,
        );
        assert_ne!(id1, id2);
        assert_eq!(registry.count(), 2);
    }

    // --- SeverityLevel tests ---

    #[test]
    fn severity_ordering() {
        assert!(SeverityLevel::Info < SeverityLevel::Warning);
        assert!(SeverityLevel::Warning < SeverityLevel::Critical);
    }

    #[test]
    fn severity_from_str_loose() {
        assert_eq!(
            SeverityLevel::from_str_loose("info"),
            Some(SeverityLevel::Info)
        );
        assert_eq!(
            SeverityLevel::from_str_loose("WARN"),
            Some(SeverityLevel::Warning)
        );
        assert_eq!(
            SeverityLevel::from_str_loose("critical"),
            Some(SeverityLevel::Critical)
        );
        assert_eq!(
            SeverityLevel::from_str_loose("error"),
            Some(SeverityLevel::Critical)
        );
        assert_eq!(SeverityLevel::from_str_loose("unknown"), None);
    }

    // --- Async EventWaiter test ---

    #[test]
    fn event_waiter_basic_construction() {
        let waiter = EventWaiter::new(WaitCondition::rule_id("test"))
            .with_timeout(Duration::from_secs(30))
            .with_filter(EventStreamFilter::builder().pane_id(1).build());
        // Just verify it builds without panic
        assert_eq!(waiter.timeout, Duration::from_secs(30));
    }

    #[test]
    fn event_waiter_all_of_waits_for_each_condition() {
        run_async_test(async {
            let bus = Arc::new(EventBus::new(16));
            let waiter = EventWaiter::new(WaitCondition::AllOf {
                conditions: vec![WaitCondition::rule_id("a"), WaitCondition::rule_id("b")],
            })
            .with_timeout(Duration::from_secs(1));
            let wait_bus = Arc::clone(&bus);

            let wait_task =
                crate::runtime_compat::task::spawn(async move { waiter.wait(&wait_bus).await });

            crate::runtime_compat::sleep(Duration::from_millis(10)).await;
            let _ = bus.publish(make_pattern_event(1, "a", Some(1)));
            crate::runtime_compat::sleep(Duration::from_millis(10)).await;
            let _ = bus.publish(make_pattern_event(1, "b", Some(2)));

            match wait_task.await.unwrap() {
                WaitResult::Matched { event, .. } => {
                    assert!(matches!(
                        event,
                        Event::PatternDetected { detection, .. } if detection.rule_id == "b"
                    ));
                }
                other => panic!("expected matched result, got {other:?}"),
            }
        });
    }

    // --- FilteredEventStream telemetry test ---

    #[test]
    fn filtered_stream_telemetry_initial() {
        let bus = EventBus::new(100);
        let stream = FilteredEventStream::new(
            &bus,
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            50,
        );
        let telem = stream.telemetry();
        assert_eq!(telem.delivered, 0);
        assert_eq!(telem.filtered_out, 0);
        assert_eq!(telem.cursor_position, 0);
    }
}
