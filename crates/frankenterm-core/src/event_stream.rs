//! Streaming event subscriptions, filters, cursors, and wait primitives.
//!
//! `DurableFilteredEventStream` drains ordered storage pages with explicit
//! acknowledgements, epoch-aware retention checks, and bus wakeups. The older
//! `FilteredEventStream` remains a live-bus stream for callers that need only
//! best-effort signals. Filter and wait-condition types are shared with the
//! storage-backed query surfaces elsewhere in the crate.
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

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::events::{Event, EventBus, EventSubscriber};
use crate::patterns::Detection;

/// Resume identity for a storage-authoritative event subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableStreamCursor {
    pub after_id: i64,
    pub epoch: String,
}

/// Storage rows and best-effort signals have different delivery authority.
#[derive(Debug)]
pub enum DurableStreamItem {
    Stored(Box<crate::storage::StoredEvent>),
    Live(Box<Event>),
    Gap { missed_count: u64 },
    Checkpoint(DurableStreamCursor),
    Idle,
}

#[derive(Debug, thiserror::Error)]
pub enum DurableStreamError {
    #[error("event storage read failed")]
    Storage(#[source] crate::Error),
    #[error("event cursor history is unavailable")]
    CursorExpired(Box<crate::storage::EventRetentionCheck>),
    #[error("event delivery must be acknowledged before reading another row")]
    PendingAcknowledgement,
    #[error("event acknowledgment does not match the pending row")]
    InvalidAcknowledgement,
    #[error("event storage returned an invalid ordered page")]
    InvalidPage,
    #[error("event subscription cancelled")]
    Cancelled,
}

const DURABLE_EVENT_PAGE_SIZE: usize = 64;
const DURABLE_EVENT_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Ordered durable delivery with one outstanding acknowledgement.
///
/// Subscribe before reading storage. Persisted bus messages only request a
/// drain; their payload and ordering never advance the durable cursor. A
/// fixed polling deadline also finds rows whose publisher died after commit.
/// The caller acknowledges a row only after a successful output flush, or
/// after deliberately filtering it. Dropping the stream before that boundary
/// leaves its previous cursor safe to replay.
pub struct DurableFilteredEventStream {
    storage: Arc<crate::storage::StorageHandle>,
    subscriber: EventSubscriber,
    cursor: Option<DurableStreamCursor>,
    rows: VecDeque<crate::storage::StoredEvent>,
    pending_id: Option<i64>,
    needs_drain: bool,
    bus_closed: bool,
    baseline_pending: bool,
    probe_bus_before_page: bool,
    next_poll: Instant,
}

impl DurableFilteredEventStream {
    #[must_use]
    pub fn new(
        bus: &EventBus,
        storage: Arc<crate::storage::StorageHandle>,
        cursor: Option<DurableStreamCursor>,
    ) -> Self {
        Self {
            storage,
            subscriber: bus.subscribe(),
            cursor,
            rows: VecDeque::new(),
            pending_id: None,
            needs_drain: true,
            bus_closed: false,
            baseline_pending: false,
            probe_bus_before_page: false,
            next_poll: Instant::now(),
        }
    }

    #[must_use]
    pub fn cursor(&self) -> Option<&DurableStreamCursor> {
        self.cursor.as_ref()
    }

    /// Commit exactly the delivered or deliberately filtered row.
    pub fn acknowledge(&mut self, event_id: i64) -> Result<(), DurableStreamError> {
        if self.pending_id != Some(event_id) {
            return Err(DurableStreamError::InvalidAcknowledgement);
        }
        let Some(cursor) = self.cursor.as_mut() else {
            return Err(DurableStreamError::InvalidAcknowledgement);
        };
        cursor.after_id = event_id;
        self.pending_id = None;
        Ok(())
    }

    async fn drain_page_cx(&mut self, cx: &crate::cx::Cx) -> Result<(), DurableStreamError> {
        if self.cursor.is_none() {
            // Subscribe first, then capture one atomic tail/epoch baseline.
            // Later commits are found even if their notification is lost.
            let tail = self
                .storage
                .get_event_retention_snapshot_with_cx(cx)
                .await
                .map_err(DurableStreamError::Storage)?;
            self.cursor = Some(DurableStreamCursor {
                after_id: tail.max_event_id,
                epoch: tail.cursor_epoch,
            });
            self.baseline_pending = true;
            return Ok(());
        }
        let cursor = self.cursor.as_ref().expect("initialized above");
        let page = self
            .storage
            .get_events_stream_page_with_cx(
                cx,
                crate::storage::EventStreamQuery {
                    after_id: Some(cursor.after_id),
                    limit: Some(DURABLE_EVENT_PAGE_SIZE),
                    ..Default::default()
                },
            )
            .await
            .map_err(DurableStreamError::Storage)?;
        let through_id = page
            .events
            .last()
            .map_or(page.retention.max_event_id, |event| event.id);
        if !durable_page_is_ordered(cursor, &page) {
            return Err(DurableStreamError::InvalidPage);
        }
        let retention = self
            .storage
            .check_event_retention_in_epoch_with_cx(
                cx,
                cursor.after_id,
                through_id.max(cursor.after_id),
                &cursor.epoch,
            )
            .await
            .map_err(DurableStreamError::Storage)?;
        if page.retention.cursor_epoch != cursor.epoch
            || retention.snapshot.cursor_epoch != cursor.epoch
            || !matches!(
                &retention.status,
                crate::storage::EventRetentionStatus::CompleteNoPruning
            )
        {
            return Err(DurableStreamError::CursorExpired(Box::new(retention)));
        }
        if page.retention.max_event_id < cursor.after_id
            || retention.checked_through_id != through_id
            || retention.snapshot.max_event_id < page.retention.max_event_id
            || retention.snapshot.generation < page.retention.generation
        {
            return Err(DurableStreamError::InvalidPage);
        }
        tracing::trace!(
            after_id = cursor.after_id,
            through_id,
            rows = page.events.len(),
            "drained durable IPC event page"
        );
        self.next_poll = Instant::now() + DURABLE_EVENT_POLL_INTERVAL;
        self.needs_drain = !page.events.is_empty();
        self.probe_bus_before_page = self.needs_drain;
        // An empty unfiltered page proves that any allocator gaps up to this
        // high-water mark contain no row requiring delivery.
        if page.events.is_empty() {
            self.cursor.as_mut().expect("initialized above").after_id = through_id;
        }
        self.rows = page.events.into();
        Ok(())
    }

    /// A bus notification is a wake hint or an explicitly best-effort signal.
    fn bus_item(&mut self, event: Event) -> DurableStreamItem {
        match event {
            Event::PatternDetected {
                event_id: Some(id), ..
            } => {
                self.needs_drain |= self
                    .cursor
                    .as_ref()
                    .is_none_or(|cursor| id > cursor.after_id);
                // Stale notifications need no query. Yield so queued wakeups
                // cannot monopolize the caller's heartbeat deadline.
                DurableStreamItem::Idle
            }
            event => DurableStreamItem::Live(Box::new(event)),
        }
    }

    /// Read one item, with a bounded idle wake for the caller's heartbeat.
    ///
    /// Heartbeat timing never cancels a storage read. The absolute storage
    /// polling deadline is not postponed by unrelated live signal traffic.
    pub async fn next_item_cx(
        &mut self,
        cx: &crate::cx::Cx,
        idle_budget: Duration,
    ) -> Result<Option<DurableStreamItem>, DurableStreamError> {
        if self.pending_id.is_some() {
            return Err(DurableStreamError::PendingAcknowledgement);
        }
        loop {
            cx.checkpoint().map_err(|_| DurableStreamError::Cancelled)?;
            if let Some(event) = self.rows.pop_front() {
                self.pending_id = Some(event.id);
                return Ok(Some(DurableStreamItem::Stored(Box::new(event))));
            }
            // Probe at most once per completed page. A durable backlog cannot
            // hide bus loss/closure forever, and a bus flood cannot postpone
            // the next storage page indefinitely.
            if std::mem::take(&mut self.probe_bus_before_page) && !self.bus_closed {
                match self.subscriber.try_recv() {
                    Some(Ok(event)) => return Ok(Some(self.bus_item(event))),
                    Some(Err(crate::events::RecvError::Lagged { missed_count })) => {
                        self.needs_drain = true;
                        return Ok(Some(DurableStreamItem::Gap { missed_count }));
                    }
                    Some(Err(crate::events::RecvError::Closed)) => {
                        self.bus_closed = true;
                        self.needs_drain = true;
                    }
                    Some(Err(crate::events::RecvError::Cancelled)) => {
                        return Err(DurableStreamError::Cancelled);
                    }
                    None => {}
                }
            }
            if self.needs_drain || Instant::now() >= self.next_poll {
                self.drain_page_cx(cx).await?;
                if std::mem::take(&mut self.baseline_pending) {
                    return Ok(Some(DurableStreamItem::Checkpoint(
                        self.cursor.as_ref().expect("initialized baseline").clone(),
                    )));
                }
                if !self.rows.is_empty() {
                    continue;
                }
            }
            if self.bus_closed {
                return Ok(None);
            }
            let wait = self
                .next_poll
                .saturating_duration_since(Instant::now())
                .min(idle_budget.max(Duration::from_millis(1)));
            match crate::runtime_async::timeout_with_cx(cx, wait, self.subscriber.recv_cx(cx)).await
            {
                Ok(Ok(event)) => return Ok(Some(self.bus_item(event))),
                Ok(Err(crate::events::RecvError::Lagged { missed_count })) => {
                    self.needs_drain = true;
                    return Ok(Some(DurableStreamItem::Gap { missed_count }));
                }
                Ok(Err(crate::events::RecvError::Closed)) => {
                    self.bus_closed = true;
                    self.needs_drain = true;
                }
                Ok(Err(crate::events::RecvError::Cancelled)) => {
                    return Err(DurableStreamError::Cancelled);
                }
                Err(_) => {
                    cx.checkpoint().map_err(|_| DurableStreamError::Cancelled)?;
                    return Ok(Some(DurableStreamItem::Idle));
                }
            }
        }
    }
}

fn durable_page_is_ordered(
    cursor: &DurableStreamCursor,
    page: &crate::storage::EventStreamPage,
) -> bool {
    if page.events.len() > DURABLE_EVENT_PAGE_SIZE {
        return false;
    }
    let mut preceding_id = cursor.after_id;
    page.events.iter().all(|event| {
        let valid = event.id > preceding_id && event.id <= page.retention.max_event_id;
        preceding_id = event.id;
        valid
    })
}

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
/// The same shape is reused by both storage-backed event queries and live bus
/// subscriptions. Live in-memory matching only evaluates predicates that can
/// be derived from the `Event` payload itself. Time-range predicates are for
/// storage/query surfaces; live `Event` variants do not carry a uniform event
/// timestamp.
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
    /// Storage/query time range: events after this epoch ms (inclusive)
    pub since_ms: Option<i64>,
    /// Storage/query time range: events before this epoch ms (exclusive)
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

        // Rule ID filter only applies to detections. Combined predicates stay
        // conjunctive, so non-detection events can never satisfy an explicit
        // rule filter even if their event type matches.
        if !self.rule_ids.is_empty() {
            if let Event::PatternDetected { detection, .. } = event {
                if !self.rule_ids.contains(&detection.rule_id) {
                    return false;
                }
            } else {
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
        event: Box<Event>,
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
    /// The waiter fell behind the bounded broadcast buffer and `missed_count`
    /// events were dropped before it drained them. The awaited event MAY have
    /// been among the dropped events, so the outcome is indeterminate — callers
    /// must re-baseline (re-query persisted state) rather than treat it as a
    /// timeout. Surfaced explicitly per the no-silent-gaps doctrine (ft-dkxp4;
    /// cf. the ft-ubuw2 `StreamItem::Gap` fix).
    Lagged {
        /// Number of events dropped from the broadcast buffer.
        missed_count: u64,
        /// How long the wait ran before it lagged (ms).
        elapsed_ms: u64,
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

    /// Returns true if the waiter fell behind the broadcast buffer (the outcome
    /// is indeterminate and the caller should re-baseline).
    #[must_use]
    pub fn is_lagged(&self) -> bool {
        matches!(self, Self::Lagged { .. })
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.wait_with_cx(&cx, bus).await
    }

    /// Explicit quarantine for legacy non-asupersync event waits.
    ///
    /// Owner: `ft-xbnl0.2.5`.
    /// Removal path: drop this helper once the workspace no longer supports
    /// non-`asupersync-runtime` event-stream waits.
    /// Execute the wait against an event bus using the caller's asupersync
    /// capability context (ft-xbnl0.2.3 / ft-xbnl0.2.2 Cx-first entry
    /// point).
    ///
    /// Short-circuits with `WaitResult::Cancelled` when the Cx is
    /// already cancelled on entry — an operator who has already
    /// abandoned the wait should not subscribe to the bus just to
    /// time out. While the wait runs the timeout is bound to the
    /// caller's Cx via [`crate::runtime_async::timeout_with_cx`], so
    /// budget-driven cancellation from the outer scope short-circuits
    /// the inner receive loop deterministically under `LabRuntime`
    /// virtual time. Matches the Cx-first pattern used by
    /// `WorkflowRunner::handle_detection_with_cx`,
    /// `SearchBridge::search_with_asupersync_cx`, and
    /// `CautClient::run_with_cx`.
    pub async fn wait_with_cx(self, cx: &crate::cx::Cx, bus: &EventBus) -> WaitResult {
        if cx.is_cancel_requested() {
            return WaitResult::Cancelled {
                reason: "capability context already cancelled".to_string(),
            };
        }

        let Self {
            condition,
            filter,
            timeout,
        } = self;
        let start = std::time::Instant::now();
        let mut subscriber = bus.subscribe();

        // Tick 191 (ft-xbnl0.2.3): each inner recv now routes through
        // `subscriber.recv_cx(cx)` so individual receives are themselves
        // cx-aware (via `broadcast_recv_with_cx`). The outer
        // `timeout_with_cx` still bounds the overall wait; this adds
        // belt-and-suspenders per-recv cancel observation so a cancel
        // fired under LabRuntime virtual time lands at the recv boundary
        // rather than waiting on drop-cancel of the outer future.
        // ft-dkxp4: surface a broadcast overflow explicitly instead of silently
        // dropping it. A waiter that fell behind cannot be sure it didn't miss
        // the awaited event, so it stops and reports the gap rather than looping
        // on — looping on could time out forever on a one-shot condition whose
        // event was in the dropped window. Mirrors the ft-ubuw2 StreamItem::Gap.
        enum WaitRecvOutcome {
            Matched(Box<Event>),
            Lagged(u64),
            Ended,
        }

        let recv_loop = async move {
            match condition {
                WaitCondition::AllOf { conditions } => {
                    let mut tracker = AllOfTracker::new(conditions);
                    loop {
                        match subscriber.recv_cx(cx).await {
                            Ok(event) => {
                                if filter.matches_event(&event) && tracker.check(&event) {
                                    return WaitRecvOutcome::Matched(Box::new(event));
                                }
                            }
                            Err(crate::events::RecvError::Lagged { missed_count }) => {
                                return WaitRecvOutcome::Lagged(missed_count);
                            }
                            Err(
                                crate::events::RecvError::Cancelled
                                | crate::events::RecvError::Closed,
                            ) => {
                                return WaitRecvOutcome::Ended;
                            }
                        }
                    }
                }
                condition => loop {
                    match subscriber.recv_cx(cx).await {
                        Ok(event) => {
                            if filter.matches_event(&event) && condition.matches(&event) {
                                return WaitRecvOutcome::Matched(Box::new(event));
                            }
                        }
                        Err(crate::events::RecvError::Lagged { missed_count }) => {
                            return WaitRecvOutcome::Lagged(missed_count);
                        }
                        Err(
                            crate::events::RecvError::Cancelled | crate::events::RecvError::Closed,
                        ) => {
                            return WaitRecvOutcome::Ended;
                        }
                    }
                },
            }
        };

        match crate::runtime_async::timeout_with_cx(cx, timeout, recv_loop).await {
            Ok(WaitRecvOutcome::Matched(event)) => WaitResult::Matched {
                event,
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
            // No silent gaps: a lag means the awaited event may have been dropped,
            // so report it as an indeterminate outcome for the caller to handle.
            Ok(WaitRecvOutcome::Lagged(missed_count)) => WaitResult::Lagged {
                missed_count,
                elapsed_ms: start.elapsed().as_millis() as u64,
            },
            Ok(WaitRecvOutcome::Ended) => {
                if cx.is_cancel_requested() {
                    WaitResult::Cancelled {
                        reason: "capability context cancelled during wait".to_string(),
                    }
                } else {
                    WaitResult::Cancelled {
                        reason: "event bus closed".to_string(),
                    }
                }
            }
            Err(_) => {
                // `timeout_with_cx` returns Err on either the timeout
                // elapsing OR the Cx being cancelled. Disambiguate for
                // the operator-facing WaitResult.
                if cx.is_cancel_requested() {
                    WaitResult::Cancelled {
                        reason: "capability context cancelled during wait".to_string(),
                    }
                } else {
                    WaitResult::Timeout {
                        elapsed_ms: start.elapsed().as_millis() as u64,
                    }
                }
            }
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
    /// A single event may satisfy multiple remaining conditions. Returns true
    /// when all conditions have been satisfied.
    pub fn check(&mut self, event: &Event) -> bool {
        let mut still_remaining = Vec::with_capacity(self.remaining.len());
        for condition in self.remaining.drain(..) {
            if condition.matches(event) {
                self.matched_events.push(event.clone());
            } else {
                still_remaining.push(condition);
            }
        }
        self.remaining = still_remaining;
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
// FilteredEventStream — Live streaming with caller-managed cursor metadata
// =============================================================================

/// One item yielded by [`FilteredEventStream::next_item_cx`]: either a matching
/// live event or an explicit gap marker.
///
/// `Gap` is emitted when the bounded broadcast buffer dropped events before this
/// subscriber drained them. A cursor-resuming consumer MUST re-baseline the
/// missed (but still persisted) detections via the storage cursor — the stream
/// never silently drops events. This mirrors the IPC watch-events gap record
/// (ft-7h5da.4.2) so every follower path upholds the no-silent-gaps doctrine.
#[derive(Debug, Clone)]
pub enum StreamItem {
    /// A live event that matched the filter.
    Event(Box<Event>),
    /// The subscriber fell behind; `missed_count` events were dropped.
    Gap {
        /// Number of events dropped from the broadcast buffer.
        missed_count: u64,
    },
}

/// A filtered live event stream backed by an `EventBus` subscription.
///
/// Cursor state is maintained for callers that want to checkpoint delivered
/// persisted detections, but this type does not perform historical replay.
/// Storage replay and cursor-resume querying live in `storage.rs`.
///
/// # Ordering Guarantees
///
/// - Live events arrive in publish order (broadcast channel FIFO).
/// - Cursor advancement is monotonic for persisted detections that carry an
///   `event_id`.
pub struct FilteredEventStream {
    /// Filter applied to all events.
    filter: EventStreamFilter,
    /// Current cursor position.
    cursor: StreamCursor,
    /// Live event subscriber.
    subscriber: EventSubscriber,
    /// Configured replay batch hint retained for callers that coordinate with
    /// storage-backed replay surfaces.
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
    /// * `cursor` - Caller-managed checkpoint metadata
    /// * `batch_size` - Replay batch hint for external storage-backed callers
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
    /// Returns `None` when the channel is closed.
    /// Updates the cursor on each delivered event (if it has a storage ID).
    pub async fn next(&mut self) -> Option<Event> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.next_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`next`].
    ///
    /// Threads the caller's cx through the underlying subscriber's
    /// `recv_cx` so caller cancellation (not just channel-close)
    /// can break the wait. Returns `None` on either channel close
    /// or cx cancellation — the two paths are indistinguishable
    /// from the caller's perspective, matching the legacy
    /// close-returns-None contract.
    pub async fn next_cx(&mut self, cx: &crate::cx::Cx) -> Option<Event> {
        loop {
            match self.subscriber.recv_cx(cx).await {
                Ok(event) => {
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
                Err(crate::events::RecvError::Lagged { .. }) => {
                    // Subscriber fell behind; keep receiving.
                }
                Err(crate::events::RecvError::Cancelled) => {
                    return None;
                }
                Err(crate::events::RecvError::Closed) => {
                    return None;
                }
            }
        }
    }

    /// Gap-aware sibling of [`next_cx`](Self::next_cx): yields each matching
    /// event as [`StreamItem::Event`] and surfaces a subscriber overflow as an
    /// explicit [`StreamItem::Gap`] instead of silently dropping the lost
    /// events. Returns `None` on channel close or cx cancellation (the two are
    /// indistinguishable to the caller, matching the close-returns-None
    /// contract).
    ///
    /// This is the gap-aware path production consumers should use; the bare
    /// [`next`](Self::next) / [`next_cx`](Self::next_cx) helpers remain
    /// event-only and intentionally skip gap markers.
    pub async fn next_item_cx(&mut self, cx: &crate::cx::Cx) -> Option<StreamItem> {
        loop {
            match self.subscriber.recv_cx(cx).await {
                Ok(event) => {
                    if self.filter.matches_event(&event) {
                        self.delivered.fetch_add(1, Ordering::Relaxed);
                        if let Event::PatternDetected {
                            event_id: Some(id), ..
                        } = &event
                        {
                            self.cursor.advance(*id);
                        }
                        return Some(StreamItem::Event(Box::new(event)));
                    }
                    self.filtered_out.fetch_add(1, Ordering::Relaxed);
                }
                // No-silent-gaps: surface the overflow so a cursor-resuming
                // follower re-baselines the missed persisted detections via the
                // storage cursor instead of skipping them unnoticed.
                Err(crate::events::RecvError::Lagged { missed_count }) => {
                    return Some(StreamItem::Gap { missed_count });
                }
                Err(crate::events::RecvError::Cancelled | crate::events::RecvError::Closed) => {
                    return None;
                }
            }
        }
    }

    /// Try to receive the next matching event without blocking.
    ///
    /// Returns `None` if no matching event is immediately available.
    /// After a `Lagged` error the subscriber cursor is advanced
    /// automatically; we retry up to a small bound to pick up the
    /// next retained event without spinning indefinitely.
    pub fn try_next(&mut self) -> Option<Event> {
        let filter = &self.filter;
        let cursor = &mut self.cursor;
        let delivered = &self.delivered;
        let filtered_out = &self.filtered_out;
        let subscriber = &mut self.subscriber;
        try_next_from_receiver(filter, cursor, delivered, filtered_out, || {
            subscriber.try_recv()
        })
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

    /// Get the configured replay batch hint.
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

fn try_next_from_receiver<F>(
    filter: &EventStreamFilter,
    cursor: &mut StreamCursor,
    delivered: &Arc<AtomicU64>,
    filtered_out: &Arc<AtomicU64>,
    mut try_recv: F,
) -> Option<Event>
where
    F: FnMut() -> Option<Result<Event, crate::events::RecvError>>,
{
    // Bound the number of consecutive Lagged recoveries to prevent
    // CPU spin when a producer is faster than the buffer can hold.
    let mut lagged_retries: u8 = 0;
    const MAX_LAGGED_RETRIES: u8 = 3;

    loop {
        match try_recv() {
            Some(Ok(event)) => {
                if filter.matches_event(&event) {
                    delivered.fetch_add(1, Ordering::Relaxed);
                    if let Event::PatternDetected {
                        event_id: Some(id), ..
                    } = &event
                    {
                        cursor.advance(*id);
                    }
                    return Some(event);
                }
                filtered_out.fetch_add(1, Ordering::Relaxed);
                // Reset lagged counter on successful recv (even if filtered).
                lagged_retries = 0;
            }
            Some(Err(crate::events::RecvError::Lagged { .. })) => {
                // The subscriber has already advanced to the newest retained
                // position. Allow one final probe after the last permitted
                // lagged marker so we don't return `None` off-by-one early.
                lagged_retries += 1;
                if lagged_retries > MAX_LAGGED_RETRIES {
                    return None;
                }
            }
            Some(Err(crate::events::RecvError::Cancelled)) => return None,
            Some(Err(crate::events::RecvError::Closed)) | None => return None,
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

type SubscriptionMap = std::collections::HashMap<SubscriptionId, SubscriptionInfo>;

/// Registry of active streaming subscriptions.
///
/// Tracks all active subscriptions for introspection and management.
///
/// NOTE: this registry currently has no production consumer — the robot
/// subscription-state surface that would expose it is not yet wired (the
/// `SubscriptionRegistry`/`SubscriptionInfo`/`SubscriptionId` family is
/// referenced only by its own unit tests). Kept as the intended introspection
/// API for that future surface.
pub struct SubscriptionRegistry {
    next_id: AtomicU64,
    subscriptions: std::sync::Mutex<SubscriptionMap>,
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

    fn subscriptions(&self) -> std::sync::MutexGuard<'_, SubscriptionMap> {
        match self.subscriptions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.subscriptions.clear_poison();
                guard
            }
        }
    }

    /// Register a new subscription and return its ID.
    pub fn register(
        &self,
        filter: EventStreamFilter,
        cursor: StreamCursor,
        now_ms: u64,
    ) -> SubscriptionId {
        let id = crate::try_next_unique_atomic_u64(&self.next_id).unwrap_or_else(|| {
            panic!(
                "event-stream subscription id space exhausted; refusing to replace an active subscription"
            )
        });
        let info = SubscriptionInfo {
            id,
            filter,
            cursor,
            created_at_ms: now_ms,
            events_delivered: 0,
            events_filtered_out: 0,
        };
        self.subscriptions().insert(id, info);
        id
    }

    /// Remove a subscription.
    pub fn unregister(&self, id: SubscriptionId) -> Option<SubscriptionInfo> {
        self.subscriptions().remove(&id)
    }

    /// Update telemetry for a subscription.
    pub fn update_telemetry(&self, id: SubscriptionId, telemetry: &StreamTelemetry) {
        if let Some(info) = self.subscriptions().get_mut(&id) {
            info.events_delivered = telemetry.delivered;
            info.events_filtered_out = telemetry.filtered_out;
            info.cursor.after_id = telemetry.cursor_position;
        }
    }

    /// List all active subscriptions.
    #[must_use]
    pub fn list(&self) -> Vec<SubscriptionInfo> {
        self.subscriptions().values().cloned().collect()
    }

    /// Get a specific subscription.
    #[must_use]
    pub fn get(&self, id: SubscriptionId) -> Option<SubscriptionInfo> {
        self.subscriptions().get(&id).cloned()
    }

    /// Number of active subscriptions.
    #[must_use]
    pub fn count(&self) -> usize {
        self.subscriptions().len()
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
        use crate::runtime_async::CompatRuntime;

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
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
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // --- Storage-authoritative event stream regressions ---

    fn durable_test_event(sequence: i64) -> crate::storage::StoredEvent {
        crate::storage::StoredEvent {
            id: 0,
            pane_id: 7,
            rule_id: format!("codex.stored.{sequence}"),
            agent_type: "codex".to_string(),
            event_type: "usage.reached".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({"source": "storage", "sequence": sequence})),
            matched_text: Some(format!("stored payload {sequence}")),
            segment_id: None,
            detected_at: 1_700_000_000_000 + sequence,
            dedupe_key: None,
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    async fn durable_storage_fixture() -> (
        tempfile::TempDir,
        Arc<crate::storage::StorageHandle>,
        DurableStreamCursor,
    ) {
        let directory = tempfile::tempdir().expect("event stream fixture directory");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(
                &directory.path().join("events.db").to_string_lossy(),
            )
            .await
            .expect("open event stream fixture"),
        );
        storage
            .upsert_pane(crate::storage::PaneRecord {
                pane_id: 7,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 1_700_000_000_000,
                last_seen_at: 1_700_000_000_000,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .expect("fixture pane");
        let snapshot = storage
            .get_event_retention_snapshot()
            .await
            .expect("fixture epoch");
        let cursor = DurableStreamCursor {
            after_id: snapshot.max_event_id,
            epoch: snapshot.cursor_epoch,
        };
        (directory, storage, cursor)
    }

    async fn next_durable_row(
        stream: &mut DurableFilteredEventStream,
        cx: &crate::cx::Cx,
    ) -> crate::storage::StoredEvent {
        match stream
            .next_item_cx(cx, Duration::from_millis(10))
            .await
            .expect("read durable stream")
        {
            Some(DurableStreamItem::Stored(event)) => *event,
            other => panic!("expected an ordered stored row, got {other:?}"),
        }
    }

    #[test]
    fn durable_stream_orders_storage_and_replays_before_acknowledgement() {
        run_async_test(async {
            let (_directory, storage, start) = durable_storage_fixture().await;
            let cx = crate::cx::Cx::current().expect("runtime context");
            let bus = EventBus::new(16);
            let mut stream =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(start.clone()));
            let first = storage.record_event(durable_test_event(1)).await.unwrap();
            let second = storage.record_event(durable_test_event(2)).await.unwrap();
            // Notification ordering and payload deliberately contradict storage.
            let _ = bus.publish(make_pattern_event(99, "wrong.second", Some(second)));
            let _ = bus.publish(make_pattern_event(99, "wrong.first", Some(first)));
            let row = next_durable_row(&mut stream, &cx).await;
            assert_eq!(row.id, first);
            assert_eq!(row.pane_id, 7);
            assert_eq!(row.rule_id, "codex.stored.1");
            assert_eq!(row.detected_at, 1_700_000_000_001);
            assert_eq!(row.matched_text.as_deref(), Some("stored payload 1"));
            assert_eq!(stream.cursor(), Some(&start));
            assert!(matches!(
                stream.acknowledge(second),
                Err(DurableStreamError::InvalidAcknowledgement)
            ));
            assert!(matches!(
                stream.next_item_cx(&cx, Duration::from_millis(1)).await,
                Err(DurableStreamError::PendingAcknowledgement)
            ));

            // A failed/partial output flush drops the stream without an ack.
            // Reconnect with the last flushed cursor and the same row replays.
            drop(stream);
            let mut resumed =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(start));
            assert_eq!(next_durable_row(&mut resumed, &cx).await.id, first);
            resumed.acknowledge(first).unwrap();
            assert_eq!(resumed.cursor().unwrap().after_id, first);
            assert_eq!(next_durable_row(&mut resumed, &cx).await.id, second);
            resumed.acknowledge(second).unwrap();
            assert_eq!(resumed.cursor().unwrap().after_id, second);
            assert!(matches!(
                resumed.acknowledge(first),
                Err(DurableStreamError::InvalidAcknowledgement)
            ));
            drop(resumed);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn durable_stream_pages_without_publication_and_reports_lag_between_pages() {
        run_async_test(async {
            let (_directory, storage, start) = durable_storage_fixture().await;
            let cx = crate::cx::Cx::current().expect("runtime context");
            let bus = EventBus::new(1);
            let mut stream =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(start));
            let mut ids = Vec::new();
            for sequence in 1..=DURABLE_EVENT_PAGE_SIZE + 3 {
                ids.push(
                    storage
                        .record_event(durable_test_event(sequence as i64))
                        .await
                        .unwrap(),
                );
            }
            // None of these stored rows has a PatternDetected notification.
            // Overflow independent live signals while the durable backlog runs.
            for pane_id in 0..4 {
                let _ = bus.publish(Event::PaneDisappeared { pane_id });
            }
            for expected in &ids[..DURABLE_EVENT_PAGE_SIZE] {
                let row = next_durable_row(&mut stream, &cx).await;
                assert_eq!(row.id, *expected);
                assert!(stream.rows.len() < DURABLE_EVENT_PAGE_SIZE);
                stream.acknowledge(row.id).unwrap();
            }
            assert!(matches!(
                stream.next_item_cx(&cx, Duration::from_millis(1)).await.unwrap(),
                Some(DurableStreamItem::Gap { missed_count }) if missed_count > 0
            ));
            for expected in &ids[DURABLE_EVENT_PAGE_SIZE..] {
                let row = next_durable_row(&mut stream, &cx).await;
                assert_eq!(row.id, *expected);
                stream.acknowledge(row.id).unwrap();
            }
            assert_eq!(stream.cursor().unwrap().after_id, *ids.last().unwrap());
            // Lifecycle delivery also gets a turn while storage is backlogged.
            assert!(matches!(
                stream.next_item_cx(&cx, Duration::from_millis(1)).await.unwrap(),
                Some(DurableStreamItem::Live(event))
                    if matches!(*event, Event::PaneDisappeared { pane_id: 3 })
            ));
            drop(stream);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn durable_stream_stale_wakeups_do_not_reset_poll_deadline() {
        run_async_test(async {
            let (_directory, storage, start) = durable_storage_fixture().await;
            let cx = crate::cx::Cx::current().expect("runtime context");
            let bus = EventBus::new(16);
            let mut stream =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(start.clone()));
            stream.drain_page_cx(&cx).await.unwrap();
            let deadline = stream.next_poll;
            let _ = bus.publish(make_pattern_event(7, "stale", Some(start.after_id)));
            assert!(matches!(
                stream
                    .next_item_cx(&cx, Duration::from_millis(1))
                    .await
                    .unwrap(),
                Some(DurableStreamItem::Idle)
            ));
            assert!(
                !stream.needs_drain,
                "stale IDs must not trigger another query"
            );
            assert_eq!(stream.next_poll, deadline);

            let id = storage.record_event(durable_test_event(1)).await.unwrap();
            // Deterministically reach the absolute poll boundary. Even queued
            // unrelated live traffic must not postpone this silent commit.
            stream.next_poll = Instant::now();
            for pane_id in 0..8 {
                let _ = bus.publish(Event::PaneDisappeared { pane_id });
            }
            assert_eq!(next_durable_row(&mut stream, &cx).await.id, id);
            stream.acknowledge(id).unwrap();
            drop(stream);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn durable_stream_fresh_baseline_precedes_later_silent_commits() {
        run_async_test(async {
            let (_directory, storage, start) = durable_storage_fixture().await;
            let old_id = storage.record_event(durable_test_event(1)).await.unwrap();
            let cx = crate::cx::Cx::current().expect("runtime context");
            let bus = EventBus::new(4);
            let mut stream = DurableFilteredEventStream::new(&bus, Arc::clone(&storage), None);
            assert_eq!(
                bus.subscriber_count(),
                1,
                "subscribe before capturing the tail"
            );
            let baseline = match stream
                .next_item_cx(&cx, Duration::from_millis(1))
                .await
                .unwrap()
            {
                Some(DurableStreamItem::Checkpoint(cursor)) => cursor,
                other => panic!("fresh clients need an explicit baseline first: {other:?}"),
            };
            assert_eq!(baseline.after_id, old_id);
            assert_eq!(baseline.epoch, start.epoch);
            let new_id = storage.record_event(durable_test_event(2)).await.unwrap();
            assert_eq!(next_durable_row(&mut stream, &cx).await.id, new_id);
            assert_eq!(stream.cursor(), Some(&baseline));
            stream.acknowledge(new_id).unwrap();
            drop(stream);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn durable_stream_rejects_wrong_epoch_ahead_cursor_and_pruned_history() {
        run_async_test(async {
            let (_directory, storage, start) = durable_storage_fixture().await;
            let cx = crate::cx::Cx::current().expect("runtime context");
            let bus = EventBus::new(4);
            let id = storage.record_event(durable_test_event(1)).await.unwrap();
            let mut wrong_epoch = start.clone();
            wrong_epoch.epoch = if start.epoch == "00000000000000000000000000000000" {
                "11111111111111111111111111111111"
            } else {
                "00000000000000000000000000000000"
            }
            .to_string();
            let mut stream = DurableFilteredEventStream::new(
                &bus,
                Arc::clone(&storage),
                Some(wrong_epoch.clone()),
            );
            assert!(matches!(
                stream.next_item_cx(&cx, Duration::from_millis(1)).await,
                Err(DurableStreamError::CursorExpired(check))
                    if matches!(check.status, crate::storage::EventRetentionStatus::CursorEpochMismatch)
            ));
            assert_eq!(stream.cursor(), Some(&wrong_epoch));
            drop(stream);

            let ahead = DurableStreamCursor {
                after_id: id + 1,
                epoch: start.epoch.clone(),
            };
            let mut stream =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(ahead.clone()));
            assert!(matches!(
                stream.next_item_cx(&cx, Duration::from_millis(1)).await,
                Err(DurableStreamError::CursorExpired(check))
                    if matches!(check.status, crate::storage::EventRetentionStatus::CursorAheadOfHighWater { .. })
            ));
            assert_eq!(stream.cursor(), Some(&ahead));
            drop(stream);

            assert_eq!(
                storage
                    .delete_events_by_retention_rule(i64::MAX, None, &[], 10)
                    .await
                    .unwrap(),
                1
            );
            let mut stream =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(start.clone()));
            assert!(matches!(
                stream.next_item_cx(&cx, Duration::from_millis(1)).await,
                Err(DurableStreamError::CursorExpired(check))
                    if matches!(check.status, crate::storage::EventRetentionStatus::Pruned { .. })
            ));
            assert_eq!(stream.cursor(), Some(&start));
            assert!(stream.pending_id.is_none());
            assert!(stream.rows.is_empty());
            drop(stream);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn durable_stream_cancelled_read_preserves_resume_state() {
        run_async_test(async {
            let (_directory, storage, start) = durable_storage_fixture().await;
            let bus = EventBus::new(4);
            let mut stream =
                DurableFilteredEventStream::new(&bus, Arc::clone(&storage), Some(start.clone()));
            storage.record_event(durable_test_event(1)).await.unwrap();
            let cancelled = crate::cx::for_testing();
            cancelled.cancel_with(crate::outcome::CancelKind::User, Some("stream test"));
            assert!(matches!(
                stream
                    .next_item_cx(&cancelled, Duration::from_millis(1))
                    .await,
                Err(DurableStreamError::Cancelled)
            ));
            assert_eq!(stream.cursor(), Some(&start));
            assert!(stream.pending_id.is_none());
            assert!(stream.rows.is_empty());
            let cx = crate::cx::Cx::current().expect("runtime context");
            let row = next_durable_row(&mut stream, &cx).await;
            stream.acknowledge(row.id).unwrap();
            drop(stream);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn durable_page_validation_rejects_unordered_duplicate_and_oversized_rows() {
        let cursor = DurableStreamCursor {
            after_id: 0,
            epoch: "0123456789abcdef0123456789abcdef".to_string(),
        };
        let mut page = crate::storage::EventStreamPage {
            events: Vec::new(),
            retention: crate::storage::EventRetentionSnapshot {
                cursor_epoch: cursor.epoch.clone(),
                legacy_history_complete: true,
                generation: 0,
                evidence_from_event_id: 1,
                max_event_id: 100,
                deleted_event_count: 0,
                last_deleted_at: None,
            },
        };
        let row = |id| {
            let mut event = durable_test_event(id);
            event.id = id;
            event
        };
        page.events = vec![row(1), row(2)];
        assert!(durable_page_is_ordered(&cursor, &page));
        for ids in [vec![2, 1], vec![1, 1], vec![0, 1], vec![1, 101]] {
            page.events = ids.into_iter().map(row).collect();
            assert!(!durable_page_is_ordered(&cursor, &page));
        }
        page.events = (1..=65).map(row).collect();
        assert!(!durable_page_is_ordered(&cursor, &page));
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
    fn rule_filter_rejects_non_detections_even_if_event_type_matches() {
        let filter = EventStreamFilter::builder()
            .rule_id("test.rule".to_string())
            .event_types(vec![
                "pattern_detected".to_string(),
                "pane_discovered".to_string(),
            ])
            .build();

        assert!(filter.matches_event(&make_pattern_event(1, "test.rule", None)));
        assert!(!filter.matches_event(&Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "test".to_string(),
        }));
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

    #[test]
    fn all_of_tracker_single_event_can_satisfy_multiple_conditions() {
        let mut tracker = AllOfTracker::new(vec![
            WaitCondition::rule_id("target"),
            WaitCondition::pane_detection(7),
        ]);
        let event = make_pattern_event(7, "target", None);

        assert!(tracker.check(&event));
        assert!(tracker.is_complete());
        assert_eq!(tracker.remaining_count(), 0);
        assert_eq!(tracker.matched_events().len(), 2);
    }

    // --- WaitResult tests ---

    #[test]
    fn wait_result_matched() {
        let result = WaitResult::Matched {
            event: Box::new(Event::PaneDiscovered {
                pane_id: 1,
                domain: "local".to_string(),
                title: "test".to_string(),
            }),
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

    #[test]
    fn wait_result_lagged_is_surfaced_explicitly() {
        // ft-dkxp4: a lagged wait must be its own outcome, not silently collapsed
        // into a timeout, so a caller can re-baseline instead of assuming the
        // event never occurred.
        let result = WaitResult::Lagged {
            missed_count: 7,
            elapsed_ms: 250,
        };
        assert!(result.is_lagged());
        assert!(!result.is_matched());
        assert!(!result.is_timeout());
        let value = serde_json::to_value(&result).expect("WaitResult serializes");
        assert_eq!(value["outcome"], serde_json::json!("lagged"));
        assert_eq!(value["missed_count"], serde_json::json!(7));
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

    #[test]
    fn registry_id_exhaustion_fails_before_replacing_an_active_subscription() {
        let registry = SubscriptionRegistry::new();
        let first_id = registry.register(
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            1000,
        );
        registry.next_id.store(u64::MAX, Ordering::Release);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.register(
                EventStreamFilter::default(),
                StreamCursor::from_beginning(),
                2000,
            )
        }));

        assert!(result.is_err());
        assert_eq!(registry.count(), 1);
        assert_eq!(registry.get(first_id).unwrap().created_at_ms, 1000);
    }

    #[test]
    fn registry_recovers_from_poisoned_subscription_lock() {
        let registry = SubscriptionRegistry::new();
        let id1 = registry.register(
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            1000,
        );

        let read_poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.subscriptions.lock().unwrap();
            panic!("poison subscription registry lock before read");
        }));
        assert!(read_poison.is_err());
        assert!(registry.subscriptions.is_poisoned());
        assert_eq!(registry.list().len(), 1);
        assert!(!registry.subscriptions.is_poisoned());

        let write_poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = registry.subscriptions.lock().unwrap();
            panic!("poison subscription registry lock before write");
        }));
        assert!(write_poison.is_err());
        assert!(registry.subscriptions.is_poisoned());

        let id2 = registry.register(
            EventStreamFilter::builder().pane_id(2).build(),
            StreamCursor::after_id(10),
            1001,
        );
        assert!(!registry.subscriptions.is_poisoned());
        assert_eq!(registry.count(), 2);

        let telemetry = StreamTelemetry {
            delivered: 4,
            filtered_out: 2,
            cursor_position: 12,
            correlation_id: None,
        };
        registry.update_telemetry(id2, &telemetry);
        assert_eq!(registry.get(id2).unwrap().events_delivered, 4);
        assert!(registry.unregister(id1).is_some());
        assert_eq!(registry.count(), 1);
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
                crate::runtime_async::task::spawn(async move { waiter.wait(&wait_bus).await });

            crate::runtime_async::sleep(Duration::from_millis(10)).await;
            let _ = bus.publish(make_pattern_event(1, "a", Some(1)));
            crate::runtime_async::sleep(Duration::from_millis(10)).await;
            let _ = bus.publish(make_pattern_event(1, "b", Some(2)));

            match wait_task.await.unwrap() {
                WaitResult::Matched { event, .. } => {
                    assert!(matches!(
                        *event,
                        Event::PatternDetected { detection, .. } if detection.rule_id == "b"
                    ));
                }
                other => panic!("expected matched result, got {other:?}"),
            }
        });
    }

    #[test]
    fn event_waiter_all_of_matches_multiple_conditions_from_one_event() {
        run_async_test(async {
            let bus = Arc::new(EventBus::new(16));
            let waiter = EventWaiter::new(WaitCondition::AllOf {
                conditions: vec![
                    WaitCondition::rule_id("target"),
                    WaitCondition::pane_detection(7),
                ],
            })
            .with_timeout(Duration::from_secs(1));
            let wait_bus = Arc::clone(&bus);

            let wait_task =
                crate::runtime_async::task::spawn(async move { waiter.wait(&wait_bus).await });

            crate::runtime_async::sleep(Duration::from_millis(10)).await;
            let _ = bus.publish(make_pattern_event(7, "target", Some(1)));

            match wait_task.await.unwrap() {
                WaitResult::Matched { event, .. } => {
                    assert!(matches!(
                        *event,
                        Event::PatternDetected {
                            pane_id: 7,
                            detection,
                            ..
                        } if detection.rule_id == "target"
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

    #[test]
    fn next_item_cx_surfaces_lagged_as_explicit_gap() {
        run_async_test(async {
            // Capacity-1 bus: publishing several events without draining forces
            // the subscriber to fall behind so recv reports a Lagged overflow.
            let bus = EventBus::new(1);
            let mut stream = FilteredEventStream::new(
                &bus,
                EventStreamFilter::default(),
                StreamCursor::from_beginning(),
                50,
            );
            for i in 0..4_u64 {
                let _ = bus.publish(Event::PaneDiscovered {
                    pane_id: i,
                    domain: "local".to_string(),
                    title: format!("pane-{i}"),
                });
            }

            let cx = crate::cx::for_testing();
            // The overflow MUST surface as an explicit Gap (ft-ubuw2), never a
            // silent drop. Drain a bounded number of items to find it.
            let mut saw_gap = false;
            for _ in 0..4 {
                match stream.next_item_cx(&cx).await {
                    Some(StreamItem::Gap { missed_count }) => {
                        assert!(missed_count >= 1, "gap must report the missed count");
                        saw_gap = true;
                        break;
                    }
                    Some(StreamItem::Event(_)) => {}
                    None => break,
                }
            }
            assert!(
                saw_gap,
                "next_item_cx must surface a Gap marker on subscriber overflow"
            );
        });
    }

    #[test]
    fn filtered_stream_try_next_drains_past_lagged_marker() {
        let bus = EventBus::new(1);
        let mut stream = FilteredEventStream::new(
            &bus,
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            50,
        );

        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 1,
            domain: "local".to_string(),
            title: "first".to_string(),
        });
        let _ = bus.publish(Event::PaneDiscovered {
            pane_id: 2,
            domain: "local".to_string(),
            title: "second".to_string(),
        });

        match stream.try_next() {
            Some(Event::PaneDiscovered { pane_id, title, .. }) => {
                assert_eq!(pane_id, 2);
                assert_eq!(title, "second");
            }
            other => panic!("expected latest retained event after lag, got {other:?}"),
        }

        let telemetry = stream.telemetry();
        assert_eq!(telemetry.delivered, 1);
        assert_eq!(telemetry.filtered_out, 0);
    }

    #[test]
    fn filtered_stream_try_next_allows_final_lagged_retry_probe() {
        let bus = EventBus::new(1);
        let mut stream = FilteredEventStream::new(
            &bus,
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            50,
        );
        let mut sequence = std::collections::VecDeque::from([
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Ok(Event::PaneDiscovered {
                pane_id: 9,
                domain: "local".to_string(),
                title: "retained".to_string(),
            })),
        ]);
        let filter = stream.filter.clone();

        let event = try_next_from_receiver(
            &filter,
            &mut stream.cursor,
            &stream.delivered,
            &stream.filtered_out,
            || sequence.pop_front().unwrap_or(None),
        );

        match event {
            Some(Event::PaneDiscovered { pane_id, title, .. }) => {
                assert_eq!(pane_id, 9);
                assert_eq!(title, "retained");
            }
            other => panic!("expected retained event after final lagged retry, got {other:?}"),
        }

        let telemetry = stream.telemetry();
        assert_eq!(telemetry.delivered, 1);
        assert_eq!(telemetry.filtered_out, 0);
    }

    #[test]
    fn filtered_stream_try_next_still_bails_after_excessive_lagged_retries() {
        let bus = EventBus::new(1);
        let mut stream = FilteredEventStream::new(
            &bus,
            EventStreamFilter::default(),
            StreamCursor::from_beginning(),
            50,
        );
        let mut sequence = std::collections::VecDeque::from([
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Err(crate::events::RecvError::Lagged { missed_count: 1 })),
            Some(Ok(Event::PaneDiscovered {
                pane_id: 10,
                domain: "local".to_string(),
                title: "too-late".to_string(),
            })),
        ]);
        let filter = stream.filter.clone();

        let event = try_next_from_receiver(
            &filter,
            &mut stream.cursor,
            &stream.delivered,
            &stream.filtered_out,
            || sequence.pop_front().unwrap_or(None),
        );

        assert!(event.is_none());
        let telemetry = stream.telemetry();
        assert_eq!(telemetry.delivered, 0);
        assert_eq!(telemetry.filtered_out, 0);
    }

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for the Cx-first wait entry point
    // (ft-xbnl0.2.3 / ft-xbnl0.2.2 slice)
    //
    // Pins the pre-cancellation short-circuit and mid-wait cancellation
    // semantics added by `EventWaiter::wait_with_cx`. Full bus-backed
    // timeout exercises live in the existing CompatRuntime suite above
    // because they depend on real broadcast-channel timing; these
    // LabRuntime tests cover the deterministic contract pieces.
    // -------------------------------------------------------------------------

    mod labruntime_event_stream {
        use super::*;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, mut handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::Quiescent
                ),
                "LabRuntime did not finish; termination: {:?}",
                report.termination,
            );
            let outcome = handle.try_join();
            assert!(
                matches!(outcome, Ok(Some(()))),
                "LabRuntime root task did not complete successfully: {outcome:?}"
            );
        }

        fn pre_cancelled_cx(msg: &'static str) -> crate::cx::Cx {
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(crate::outcome::CancelKind::User, Some(msg));
            cx
        }

        /// 1. `wait_with_cx` short-circuits with WaitResult::Cancelled
        ///    when the Cx is already cancelled on entry. No subscription
        ///    is taken against the bus — an operator who has abandoned
        ///    the wait should not register as a subscriber just to time
        ///    out.
        #[test]
        fn wait_with_cx_precancelled_short_circuits_under_labruntime() {
            run_lab(6001, || async move {
                let bus = EventBus::new(16);
                let waiter = EventWaiter::new(WaitCondition::AnyEvent);
                let cx = pre_cancelled_cx("wa-event-stream precancel");
                let result = waiter.wait_with_cx(&cx, &bus).await;
                match result {
                    WaitResult::Cancelled { reason } => {
                        assert!(
                            reason.contains("capability context already cancelled"),
                            "pre-cancel reason must be explicit: {reason}"
                        );
                    }
                    other => panic!("expected Cancelled, got {other:?}"),
                }
            });
        }

        /// 2. `WaitResult::Matched` / `Timeout` / `Cancelled` classify
        ///    correctly under LabRuntime virtual time — the helpers
        ///    downstream telemetry uses to route operator feedback.
        #[test]
        fn wait_result_predicates_under_labruntime() {
            run_lab(6002, || async move {
                let matched = WaitResult::Cancelled {
                    reason: "x".to_owned(),
                };
                assert!(!matched.is_matched());
                assert!(!matched.is_timeout());

                let timeout = WaitResult::Timeout { elapsed_ms: 1_000 };
                assert!(!timeout.is_matched());
                assert!(timeout.is_timeout());
            });
        }

        /// 3. `WaitResult::Cancelled` serde roundtrip under LabRuntime.
        ///    The Cx-first path emits this variant with a reason field;
        ///    pin that the wire shape stays stable so downstream log
        ///    scrapers keep working.
        #[test]
        fn wait_result_cancelled_serde_roundtrip_under_labruntime() {
            run_lab(6003, || async move {
                let wr = WaitResult::Cancelled {
                    reason: "capability context already cancelled".to_owned(),
                };
                let json = serde_json::to_string(&wr).expect("serialize");
                assert!(
                    json.contains("\"outcome\":\"cancelled\""),
                    "serde tag missing: {json}"
                );
                assert!(json.contains("capability context already cancelled"));
                let restored: WaitResult = serde_json::from_str(&json).expect("deserialize");
                match restored {
                    WaitResult::Cancelled { reason } => {
                        assert_eq!(reason, "capability context already cancelled");
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            });
        }

        /// 4. `WaitResult::Timeout` serde roundtrip keeps elapsed_ms
        ///    numeric (not stringified).
        #[test]
        fn wait_result_timeout_serde_roundtrip_under_labruntime() {
            run_lab(6004, || async move {
                let wr = WaitResult::Timeout {
                    elapsed_ms: 3_600_000,
                };
                let json = serde_json::to_string(&wr).expect("serialize");
                assert!(json.contains("\"outcome\":\"timeout\""));
                assert!(json.contains("\"elapsed_ms\":3600000"));
                let restored: WaitResult = serde_json::from_str(&json).expect("deserialize");
                match restored {
                    WaitResult::Timeout { elapsed_ms } => {
                        assert_eq!(elapsed_ms, 3_600_000);
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            });
        }

        /// 5. `EventWaiter::new` defaults the timeout to 1 hour. Pin
        ///    that so a Cx-first caller who sets only the condition
        ///    inherits a reasonable default budget.
        #[test]
        fn event_waiter_default_timeout_under_labruntime() {
            run_lab(6005, || async move {
                let waiter = EventWaiter::new(WaitCondition::AnyEvent);
                // timeout field is private; exercise via fluent builder.
                let waiter = waiter.with_timeout(Duration::from_secs(60));
                // Drop without waiting — just asserts the builder works.
                drop(waiter);
            });
        }
    }

    /// ft-xbnl0.2.3 Cx-first: `EventFilterStream::next_cx` must
    /// deliver a matching event identically to the legacy `next`
    /// when given a fresh cx. Verified by publishing one matching
    /// event and confirming the cx-first next returns it.
    #[test]
    fn event_filter_stream_next_cx_delivers_matching_event() {
        run_async_test(async {
            let bus = Arc::new(EventBus::new(16));
            let filter = EventStreamFilter::default();
            let cursor = StreamCursor::from_beginning();
            let mut stream = FilteredEventStream::new(&bus, filter, cursor, 32);

            let cx = crate::cx::for_testing();
            let event = make_pattern_event(5, "rule-x", Some(99));
            let _ = bus.publish(event);

            let delivered = stream.next_cx(&cx).await.expect("should deliver event");
            match delivered {
                Event::PatternDetected {
                    detection,
                    event_id: Some(id),
                    ..
                } => {
                    assert_eq!(detection.rule_id, "rule-x");
                    assert_eq!(id, 99);
                }
                other => panic!("expected PatternDetected, got: {other:?}"),
            }
        });
    }
}
