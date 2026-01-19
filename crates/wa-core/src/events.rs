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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::patterns::Detection;

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
    PatternDetected { pane_id: u64, detection: Detection },

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
            | Self::WorkflowStarted { pane_id, .. } => Some(*pane_id),
            Self::WorkflowStep { .. } | Self::WorkflowCompleted { .. } => None,
        }
    }
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
        self.metrics.events_published.fetch_add(1, Ordering::Relaxed);
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
            | Event::WorkflowCompleted { .. } => {
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
        match sender.send(event) {
            Ok(count) => {
                Self::record_timestamp(times, self.capacity);
                count
            }
            Err(_) => 0,
        }
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
        let guard = times.lock().ok()?;
        let oldest = guard.front()?;
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
            Self::Lagged { missed_count } => write!(f, "subscriber lagged, missed {missed_count} events"),
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
}
