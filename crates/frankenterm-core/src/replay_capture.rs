//! Replay capture adapter — bridges the ingest/event pipeline to
//! `.ftreplay`-compatible [`RecorderEvent`] records.
//!
//! The adapter converts [`CapturedSegment`] egress events and lifecycle/control
//! markers into fully populated [`RecorderEvent`] records with deterministic
//! event IDs and merge keys assigned at capture time.
//!
//! # Architecture
//!
//! ```text
//! ingest.rs ──► CapturedSegment ──► CaptureAdapter ──► RecorderEvent
//! events.rs ──► Event           ──► CaptureAdapter ──► RecorderEvent
//! ```
//!
//! The adapter is designed as an observer (tap) that does not modify the
//! upstream pipeline. It is zero-cost when no sink is attached.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// Re-export decision types from replay_decision_graph for callers that
// reference `crate::replay_capture::DecisionEvent` / `DecisionType`.
pub use crate::replay_decision_graph::{DecisionEvent, DecisionType};

use crate::event_id::{RecorderMergeKey, StreamKind, generate_event_id_v1};
use crate::ingest::CapturedSegment;
use crate::recording::{
    EgressEvent, EgressTap, GlobalSequence, IngressEvent, IngressOutcome, IngressTap,
    RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
    RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderLifecyclePhase,
    RecorderRedactionLevel, RecorderTextEncoding, captured_kind_to_segment, epoch_ms_now,
};

// ---------------------------------------------------------------------------
// Capture sink trait
// ---------------------------------------------------------------------------

/// Receiver for captured [`RecorderEvent`] records.
///
/// Implementations should be fast and non-blocking. Heavy work (disk I/O,
/// network) should be offloaded to a background task via a channel.
pub trait CaptureSink: Send + Sync {
    /// Called for each captured event. Must not block.
    fn on_event(&self, event: RecorderEvent, merge_key: RecorderMergeKey);
}

/// No-op sink that discards all events (zero overhead).
pub struct NoopCaptureSink;

impl CaptureSink for NoopCaptureSink {
    #[inline]
    fn on_event(&self, _event: RecorderEvent, _merge_key: RecorderMergeKey) {}
}

/// Collecting sink that stores all events for testing.
#[derive(Debug, Default)]
pub struct CollectingCaptureSink {
    events: Mutex<Vec<(RecorderEvent, RecorderMergeKey)>>,
}

impl CollectingCaptureSink {
    /// Create a new empty collecting sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a snapshot of all collected events.
    pub fn events(&self) -> Vec<(RecorderEvent, RecorderMergeKey)> {
        self.events.lock().unwrap().clone()
    }

    /// Return just the recorder events without merge keys.
    pub fn recorder_events(&self) -> Vec<RecorderEvent> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(e, _)| e.clone())
            .collect()
    }

    /// Return the number of collected events.
    pub fn len(&self) -> usize {
        self.events.lock().unwrap().len()
    }

    /// Return true if no events have been collected.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all collected events.
    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}

impl CaptureSink for CollectingCaptureSink {
    fn on_event(&self, event: RecorderEvent, merge_key: RecorderMergeKey) {
        self.events.lock().unwrap().push((event, merge_key));
    }
}

// ---------------------------------------------------------------------------
// Capture adapter
// ---------------------------------------------------------------------------

/// Configuration for the capture adapter.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Session identifier to stamp on all events.
    pub session_id: Option<String>,
    /// Default event source for egress events.
    pub default_source: RecorderEventSource,
    /// Whether capture is enabled (can be toggled at runtime).
    pub enabled: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            session_id: None,
            default_source: RecorderEventSource::WeztermMux,
            enabled: true,
        }
    }
}

/// Runtime capture adapter that converts pipeline events into
/// [`RecorderEvent`] records with deterministic ordering metadata.
///
/// The adapter maintains per-pane sequence counters and a global sequence
/// counter for cross-pane merge ordering. Events are emitted to a
/// [`CaptureSink`] which can buffer, persist, or discard them.
pub struct CaptureAdapter {
    /// Sink receiving captured events.
    sink: Arc<dyn CaptureSink>,
    /// Global (cross-pane) monotonic sequence counter.
    global_seq: Arc<GlobalSequence>,
    /// Per-pane sequence counters for recorder events.
    pane_sequences: Mutex<HashMap<u64, AtomicU64>>,
    /// Runtime enable/disable flag.
    enabled: AtomicBool,
    /// Session identifier stamped on all events.
    session_id: Option<String>,
    /// Default source for egress events.
    default_source: RecorderEventSource,
    /// Total events captured (monotonic counter for diagnostics).
    total_captured: AtomicU64,
}

impl CaptureAdapter {
    /// Create a new capture adapter with the given sink and configuration.
    pub fn new(sink: Arc<dyn CaptureSink>, config: CaptureConfig) -> Self {
        Self {
            sink,
            global_seq: Arc::new(GlobalSequence::new()),
            pane_sequences: Mutex::new(HashMap::new()),
            enabled: AtomicBool::new(config.enabled),
            session_id: config.session_id,
            default_source: config.default_source,
            total_captured: AtomicU64::new(0),
        }
    }

    /// Create a capture adapter with a no-op sink (zero overhead).
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(
            Arc::new(NoopCaptureSink),
            CaptureConfig {
                enabled: false,
                ..Default::default()
            },
        )
    }

    /// Enable or disable capture at runtime.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    /// Check if capture is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Return the total number of events captured so far.
    pub fn total_captured(&self) -> u64 {
        self.total_captured.load(Ordering::Relaxed)
    }

    /// Return a reference to the global sequence counter.
    pub fn global_sequence(&self) -> &GlobalSequence {
        &self.global_seq
    }

    /// Return a clone of the shared global sequence counter.
    pub fn global_sequence_handle(&self) -> Arc<GlobalSequence> {
        Arc::clone(&self.global_seq)
    }

    /// Get or create a per-pane sequence counter, returning the next value.
    fn next_pane_seq(&self, pane_id: u64) -> u64 {
        let mut map = self.pane_sequences.lock().unwrap();
        let counter = map.entry(pane_id).or_insert_with(|| AtomicU64::new(0));
        counter.fetch_add(1, Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // Egress capture (from CapturedSegment)
    // -----------------------------------------------------------------------

    /// Capture an egress event from a [`CapturedSegment`].
    ///
    /// Converts the segment into a [`RecorderEvent`] with
    /// [`RecorderEventPayload::EgressOutput`], assigns a deterministic event ID,
    /// and emits to the sink.
    pub fn capture_egress(&self, segment: &CapturedSegment) {
        if !self.is_enabled() {
            return;
        }

        let (segment_kind, is_gap) = captured_kind_to_segment(&segment.kind);
        let occurred_at_ms = if segment.captured_at >= 0 {
            segment.captured_at as u64
        } else {
            epoch_ms_now()
        };
        let recorded_at_ms = epoch_ms_now();
        let sequence = self.next_pane_seq(segment.pane_id);

        let payload = RecorderEventPayload::EgressOutput {
            text: segment.content.clone(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind,
            is_gap,
        };

        let mut event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: String::new(), // computed below
            pane_id: segment.pane_id,
            session_id: self.session_id.clone(),
            workflow_id: None,
            correlation_id: None,
            source: self.default_source,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload,
        };

        // Deterministic event ID from content
        event.event_id = generate_event_id_v1(&event);

        let merge_key = RecorderMergeKey {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        };

        self.total_captured.fetch_add(1, Ordering::Relaxed);
        self.sink.on_event(event, merge_key);
    }

    /// Capture an egress event from an [`EgressEvent`] (pre-built metadata).
    ///
    /// This path is used when the upstream already has an EgressEvent struct
    /// (e.g., from an existing EgressTap implementation).
    pub fn capture_egress_event(&self, egress: &EgressEvent) {
        if !self.is_enabled() {
            return;
        }

        let recorded_at_ms = epoch_ms_now();
        let sequence = self.next_pane_seq(egress.pane_id);

        let payload = RecorderEventPayload::EgressOutput {
            text: egress.text.clone(),
            encoding: egress.encoding,
            redaction: egress.redaction,
            segment_kind: egress.segment_kind,
            is_gap: egress.is_gap,
        };

        let mut event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: String::new(),
            pane_id: egress.pane_id,
            session_id: self.session_id.clone(),
            workflow_id: None,
            correlation_id: None,
            source: self.default_source,
            occurred_at_ms: egress.occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload,
        };

        event.event_id = generate_event_id_v1(&event);

        let merge_key = RecorderMergeKey {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        };

        self.total_captured.fetch_add(1, Ordering::Relaxed);
        self.sink.on_event(event, merge_key);
    }

    // -----------------------------------------------------------------------
    // Lifecycle capture
    // -----------------------------------------------------------------------

    /// Capture a lifecycle event (pane open/close, capture start/stop).
    pub fn capture_lifecycle(
        &self,
        pane_id: u64,
        phase: RecorderLifecyclePhase,
        reason: Option<String>,
        details: serde_json::Value,
    ) {
        if !self.is_enabled() {
            return;
        }

        let occurred_at_ms = epoch_ms_now();
        let recorded_at_ms = occurred_at_ms;
        let sequence = self.next_pane_seq(pane_id);

        let payload = RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: phase,
            reason,
            details,
        };

        let mut event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: String::new(),
            pane_id,
            session_id: self.session_id.clone(),
            workflow_id: None,
            correlation_id: None,
            source: self.default_source,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload,
        };

        event.event_id = generate_event_id_v1(&event);

        let merge_key = RecorderMergeKey {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        };

        self.total_captured.fetch_add(1, Ordering::Relaxed);
        self.sink.on_event(event, merge_key);
    }

    // -----------------------------------------------------------------------
    // Control marker capture
    // -----------------------------------------------------------------------

    /// Capture a control marker event (resize, prompt boundary, etc.).
    pub fn capture_control(
        &self,
        pane_id: u64,
        marker_type: crate::recording::RecorderControlMarkerType,
        details: serde_json::Value,
    ) {
        if !self.is_enabled() {
            return;
        }

        let occurred_at_ms = epoch_ms_now();
        let recorded_at_ms = occurred_at_ms;
        let sequence = self.next_pane_seq(pane_id);

        let payload = RecorderEventPayload::ControlMarker {
            control_marker_type: marker_type,
            details,
        };

        let mut event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: String::new(),
            pane_id,
            session_id: self.session_id.clone(),
            workflow_id: None,
            correlation_id: None,
            source: self.default_source,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload,
        };

        event.event_id = generate_event_id_v1(&event);

        let merge_key = RecorderMergeKey {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        };

        self.total_captured.fetch_add(1, Ordering::Relaxed);
        self.sink.on_event(event, merge_key);
    }

    // -----------------------------------------------------------------------
    // Ingress capture
    // -----------------------------------------------------------------------

    /// Capture an ingress (input) event.
    pub fn capture_ingress(
        &self,
        pane_id: u64,
        text: String,
        ingress_kind: crate::recording::RecorderIngressKind,
        source: RecorderEventSource,
        workflow_id: Option<String>,
        causality: RecorderEventCausality,
    ) {
        if !self.is_enabled() {
            return;
        }

        let occurred_at_ms = epoch_ms_now();
        let recorded_at_ms = occurred_at_ms;
        let sequence = self.next_pane_seq(pane_id);

        let payload = RecorderEventPayload::IngressText {
            text,
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind,
        };

        let mut event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: String::new(),
            pane_id,
            session_id: self.session_id.clone(),
            workflow_id,
            correlation_id: None,
            source,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality,
            payload,
        };

        event.event_id = generate_event_id_v1(&event);

        let merge_key = RecorderMergeKey {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        };

        self.total_captured.fetch_add(1, Ordering::Relaxed);
        self.sink.on_event(event, merge_key);
    }

    /// Capture a decision event (policy evaluation, workflow step, etc.).
    ///
    /// Converts the [`DecisionEvent`] into a [`RecorderEvent`] with a
    /// `ControlMarker` payload and forwards it to the configured sink.
    pub fn capture_decision(
        &self,
        source: RecorderEventSource,
        correlation_id: Option<String>,
        decision: DecisionEvent,
    ) {
        if !self.is_enabled() {
            return;
        }

        let pane_id = decision.pane_id;
        let occurred_at_ms = decision.timestamp_ms;
        let recorded_at_ms = epoch_ms_now();
        let sequence = self.next_pane_seq(pane_id);

        let details = serde_json::to_value(&decision).unwrap_or_default();

        let payload = RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::PolicyDecision,
            details,
        };

        let mut event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.into(),
            event_id: String::new(),
            pane_id,
            session_id: self.session_id.clone(),
            workflow_id: None,
            correlation_id,
            source,
            occurred_at_ms,
            recorded_at_ms,
            sequence,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload,
        };

        event.event_id = generate_event_id_v1(&event);

        let merge_key = RecorderMergeKey {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        };

        self.total_captured.fetch_add(1, Ordering::Relaxed);
        self.sink.on_event(event, merge_key);
    }
}

// ---------------------------------------------------------------------------
// EgressTap implementation — allows CaptureAdapter to be used as an EgressTap
// ---------------------------------------------------------------------------

impl EgressTap for CaptureAdapter {
    fn on_egress(&self, event: EgressEvent) {
        self.capture_egress_event(&event);
    }
}

// ---------------------------------------------------------------------------
// IngressTap implementation — allows CaptureAdapter to be used as an IngressTap
// ---------------------------------------------------------------------------

impl IngressTap for CaptureAdapter {
    fn on_ingress(&self, event: IngressEvent) {
        self.capture_ingress(
            event.pane_id,
            event.text.clone(),
            event.ingress_kind,
            event.source,
            event.workflow_id.clone(),
            RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
        );

        use crate::recording::RecorderControlMarkerType;
        let (marker_type, details) = match event.outcome {
            IngressOutcome::Allowed => (
                RecorderControlMarkerType::PolicyDecision,
                serde_json::json!({
                    "outcome": "allow",
                    "ingress_kind": event.ingress_kind,
                }),
            ),
            IngressOutcome::Denied { reason } => (
                RecorderControlMarkerType::PolicyDecision,
                serde_json::json!({
                    "outcome": "deny",
                    "reason": reason,
                    "ingress_kind": event.ingress_kind,
                }),
            ),
            IngressOutcome::RequiresApproval => (
                RecorderControlMarkerType::ApprovalCheckpoint,
                serde_json::json!({
                    "outcome": "requires_approval",
                    "ingress_kind": event.ingress_kind,
                }),
            ),
            IngressOutcome::Error { error } => (
                RecorderControlMarkerType::PolicyDecision,
                serde_json::json!({
                    "outcome": "error",
                    "error": error,
                    "ingress_kind": event.ingress_kind,
                }),
            ),
        };

        self.capture_control(event.pane_id, marker_type, details);
    }
}

// ---------------------------------------------------------------------------
// Shared handle alias
// ---------------------------------------------------------------------------

/// Convenience alias for a thread-safe capture adapter handle.
pub type SharedCaptureAdapter = Arc<CaptureAdapter>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{RecorderControlMarkerType, RecorderIngressKind, RecorderSegmentKind};
    use serde_json::json;

    fn make_adapter() -> (Arc<CollectingCaptureSink>, CaptureAdapter) {
        let sink = Arc::new(CollectingCaptureSink::new());
        let config = CaptureConfig {
            session_id: Some("test-session-001".into()),
            ..Default::default()
        };
        let adapter = CaptureAdapter::new(sink.clone(), config);
        (sink, adapter)
    }

    fn make_segment(pane_id: u64, content: &str, seq: u64) -> CapturedSegment {
        CapturedSegment {
            pane_id,
            seq,
            content: content.to_string(),
            kind: crate::ingest::CapturedSegmentKind::Delta,
            captured_at: 1700000000000,
        }
    }

    fn make_gap_segment(pane_id: u64, reason: &str, seq: u64) -> CapturedSegment {
        CapturedSegment {
            pane_id,
            seq,
            content: String::new(),
            kind: crate::ingest::CapturedSegmentKind::Gap {
                reason: reason.to_string(),
            },
            captured_at: 1700000000000,
        }
    }

    // --- Basic egress capture ---

    #[test]
    fn test_capture_egress_delta_produces_recorder_event() {
        let (sink, adapter) = make_adapter();
        let seg = make_segment(42, "hello world", 0);
        adapter.capture_egress(&seg);

        assert_eq!(sink.len(), 1);
        let events = sink.recorder_events();
        let evt = &events[0];
        assert_eq!(evt.pane_id, 42);
        assert_eq!(evt.schema_version, RECORDER_EVENT_SCHEMA_VERSION_V1);
        assert_eq!(evt.session_id, Some("test-session-001".into()));
        match &evt.payload {
            RecorderEventPayload::EgressOutput {
                text,
                is_gap,
                segment_kind,
                ..
            } => {
                assert_eq!(text, "hello world");
                assert!(!is_gap);
                assert_eq!(*segment_kind, RecorderSegmentKind::Delta);
            }
            _ => panic!("expected EgressOutput payload"),
        }
    }

    #[test]
    fn test_capture_egress_gap_produces_gap_event() {
        let (sink, adapter) = make_adapter();
        let seg = make_gap_segment(42, "stream_overflow", 0);
        adapter.capture_egress(&seg);

        assert_eq!(sink.len(), 1);
        let evt = &sink.recorder_events()[0];
        match &evt.payload {
            RecorderEventPayload::EgressOutput {
                is_gap,
                segment_kind,
                ..
            } => {
                assert!(*is_gap);
                assert_eq!(*segment_kind, RecorderSegmentKind::Gap);
            }
            _ => panic!("expected EgressOutput"),
        }
    }

    #[test]
    fn test_capture_egress_assigns_deterministic_event_id() {
        let (sink, adapter) = make_adapter();
        let seg = make_segment(1, "deterministic", 0);
        adapter.capture_egress(&seg);

        let evt = &sink.recorder_events()[0];
        assert!(!evt.event_id.is_empty());
        assert_eq!(evt.event_id.len(), 64); // SHA-256 hex
    }

    #[test]
    fn test_event_id_differs_for_different_content() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "aaa", 0));
        adapter.capture_egress(&make_segment(1, "bbb", 1));

        let events = sink.recorder_events();
        assert_ne!(events[0].event_id, events[1].event_id);
    }

    #[test]
    fn test_event_id_same_content_same_pane_different_sequence() {
        let (sink, adapter) = make_adapter();
        // Same content but different sequence → different event_id
        adapter.capture_egress(&make_segment(1, "same", 0));
        adapter.capture_egress(&make_segment(1, "same", 1));

        let events = sink.recorder_events();
        // sequence is assigned by adapter (internal counter), so IDs differ
        assert_ne!(events[0].event_id, events[1].event_id);
    }

    // --- Sequence numbering ---

    #[test]
    fn test_per_pane_sequences_are_monotonic() {
        let (sink, adapter) = make_adapter();
        for i in 0..5 {
            adapter.capture_egress(&make_segment(1, &format!("msg{i}"), i));
        }

        let events = sink.recorder_events();
        for (i, event) in events.iter().enumerate().take(5) {
            assert_eq!(event.sequence, i as u64);
        }
    }

    #[test]
    fn test_different_panes_have_independent_sequences() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "p1a", 0));
        adapter.capture_egress(&make_segment(2, "p2a", 0));
        adapter.capture_egress(&make_segment(1, "p1b", 1));
        adapter.capture_egress(&make_segment(2, "p2b", 1));

        let events = sink.recorder_events();
        // Pane 1: seq 0, 1
        assert_eq!(events[0].sequence, 0);
        assert_eq!(events[2].sequence, 1);
        // Pane 2: seq 0, 1
        assert_eq!(events[1].sequence, 0);
        assert_eq!(events[3].sequence, 1);
    }

    // --- Merge key ---

    #[test]
    fn test_merge_key_has_correct_stream_kind() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "test", 0));

        let (_, mk) = &sink.events()[0];
        assert_eq!(mk.stream_kind, StreamKind::Egress);
    }

    #[test]
    fn test_merge_key_matches_event_fields() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "test", 0));

        let (evt, mk) = &sink.events()[0];
        assert_eq!(mk.pane_id, evt.pane_id);
        assert_eq!(mk.sequence, evt.sequence);
        assert_eq!(mk.event_id, evt.event_id);
        assert_eq!(mk.recorded_at_ms, evt.recorded_at_ms);
    }

    // --- Lifecycle capture ---

    #[test]
    fn test_capture_lifecycle_pane_opened() {
        let (sink, adapter) = make_adapter();
        adapter.capture_lifecycle(
            10,
            RecorderLifecyclePhase::PaneOpened,
            None,
            json!({"title": "bash"}),
        );

        assert_eq!(sink.len(), 1);
        let evt = &sink.recorder_events()[0];
        assert_eq!(evt.pane_id, 10);
        match &evt.payload {
            RecorderEventPayload::LifecycleMarker {
                lifecycle_phase,
                details,
                ..
            } => {
                assert_eq!(*lifecycle_phase, RecorderLifecyclePhase::PaneOpened);
                assert_eq!(details["title"], "bash");
            }
            _ => panic!("expected LifecycleMarker"),
        }
    }

    #[test]
    fn test_capture_lifecycle_pane_closed_with_reason() {
        let (sink, adapter) = make_adapter();
        adapter.capture_lifecycle(
            10,
            RecorderLifecyclePhase::PaneClosed,
            Some("user_exit".into()),
            json!({}),
        );

        let evt = &sink.recorder_events()[0];
        match &evt.payload {
            RecorderEventPayload::LifecycleMarker {
                lifecycle_phase,
                reason,
                ..
            } => {
                assert_eq!(*lifecycle_phase, RecorderLifecyclePhase::PaneClosed);
                assert_eq!(reason.as_deref(), Some("user_exit"));
            }
            _ => panic!("expected LifecycleMarker"),
        }
    }

    #[test]
    fn test_lifecycle_has_lifecycle_stream_kind() {
        let (sink, adapter) = make_adapter();
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::CaptureStarted, None, json!({}));

        let (_, mk) = &sink.events()[0];
        assert_eq!(mk.stream_kind, StreamKind::Lifecycle);
    }

    #[test]
    fn test_capture_started_and_stopped() {
        let (sink, adapter) = make_adapter();
        adapter.capture_lifecycle(0, RecorderLifecyclePhase::CaptureStarted, None, json!({}));
        adapter.capture_lifecycle(0, RecorderLifecyclePhase::CaptureStopped, None, json!({}));
        assert_eq!(sink.len(), 2);
    }

    // --- Control marker capture ---

    #[test]
    fn test_capture_control_resize() {
        let (sink, adapter) = make_adapter();
        adapter.capture_control(
            5,
            RecorderControlMarkerType::Resize,
            json!({"cols": 120, "rows": 40}),
        );

        assert_eq!(sink.len(), 1);
        let evt = &sink.recorder_events()[0];
        match &evt.payload {
            RecorderEventPayload::ControlMarker {
                control_marker_type,
                details,
            } => {
                assert_eq!(*control_marker_type, RecorderControlMarkerType::Resize);
                assert_eq!(details["cols"], 120);
                assert_eq!(details["rows"], 40);
            }
            _ => panic!("expected ControlMarker"),
        }
    }

    #[test]
    fn test_capture_control_prompt_boundary() {
        let (sink, adapter) = make_adapter();
        adapter.capture_control(
            5,
            RecorderControlMarkerType::PromptBoundary,
            json!({"cwd": "/home/user"}),
        );

        let evt = &sink.recorder_events()[0];
        match &evt.payload {
            RecorderEventPayload::ControlMarker {
                control_marker_type,
                ..
            } => {
                assert_eq!(
                    *control_marker_type,
                    RecorderControlMarkerType::PromptBoundary
                );
            }
            _ => panic!("expected ControlMarker"),
        }
    }

    #[test]
    fn test_control_has_control_stream_kind() {
        let (sink, adapter) = make_adapter();
        adapter.capture_control(1, RecorderControlMarkerType::Resize, json!({}));

        let (_, mk) = &sink.events()[0];
        assert_eq!(mk.stream_kind, StreamKind::Control);
    }

    // --- Ingress capture ---

    #[test]
    fn test_capture_ingress_send_text() {
        let (sink, adapter) = make_adapter();
        adapter.capture_ingress(
            7,
            "ls -la\n".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::RobotMode,
            None,
            RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
        );

        assert_eq!(sink.len(), 1);
        let evt = &sink.recorder_events()[0];
        assert_eq!(evt.pane_id, 7);
        assert_eq!(evt.source, RecorderEventSource::RobotMode);
        match &evt.payload {
            RecorderEventPayload::IngressText {
                text, ingress_kind, ..
            } => {
                assert_eq!(text, "ls -la\n");
                assert_eq!(*ingress_kind, RecorderIngressKind::SendText);
            }
            _ => panic!("expected IngressText"),
        }
    }

    #[test]
    fn test_capture_ingress_workflow_action() {
        let (sink, adapter) = make_adapter();
        adapter.capture_ingress(
            7,
            "approve".into(),
            RecorderIngressKind::WorkflowAction,
            RecorderEventSource::WorkflowEngine,
            Some("wf-001".into()),
            RecorderEventCausality {
                parent_event_id: Some("parent-evt".into()),
                trigger_event_id: None,
                root_event_id: None,
            },
        );

        let evt = &sink.recorder_events()[0];
        assert_eq!(evt.workflow_id, Some("wf-001".into()));
        assert_eq!(evt.causality.parent_event_id, Some("parent-evt".into()));
    }

    #[test]
    fn test_ingress_has_ingress_stream_kind() {
        let (sink, adapter) = make_adapter();
        adapter.capture_ingress(
            1,
            "x".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::OperatorAction,
            None,
            RecorderEventCausality::default(),
        );

        let (_, mk) = &sink.events()[0];
        assert_eq!(mk.stream_kind, StreamKind::Ingress);
    }

    // --- Enable/disable ---

    #[test]
    fn test_disabled_adapter_captures_nothing() {
        let (sink, adapter) = make_adapter();
        adapter.set_enabled(false);
        adapter.capture_egress(&make_segment(1, "should be dropped", 0));
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::PaneOpened, None, json!({}));
        adapter.capture_control(1, RecorderControlMarkerType::Resize, json!({}));
        adapter.capture_ingress(
            1,
            "x".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::OperatorAction,
            None,
            RecorderEventCausality::default(),
        );
        assert!(sink.is_empty());
    }

    #[test]
    fn test_toggle_enabled_at_runtime() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "first", 0));
        assert_eq!(sink.len(), 1);

        adapter.set_enabled(false);
        adapter.capture_egress(&make_segment(1, "dropped", 1));
        assert_eq!(sink.len(), 1);

        adapter.set_enabled(true);
        adapter.capture_egress(&make_segment(1, "third", 2));
        assert_eq!(sink.len(), 2);
    }

    #[test]
    fn test_disabled_constructor_captures_nothing() {
        let adapter = CaptureAdapter::disabled();
        adapter.capture_egress(&make_segment(1, "nope", 0));
        assert_eq!(adapter.total_captured(), 0);
    }

    // --- Total captured counter ---

    #[test]
    fn test_total_captured_increments() {
        let (_sink, adapter) = make_adapter();
        assert_eq!(adapter.total_captured(), 0);
        adapter.capture_egress(&make_segment(1, "a", 0));
        assert_eq!(adapter.total_captured(), 1);
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::PaneOpened, None, json!({}));
        assert_eq!(adapter.total_captured(), 2);
        adapter.capture_control(1, RecorderControlMarkerType::Resize, json!({}));
        assert_eq!(adapter.total_captured(), 3);
    }

    // --- Schema version ---

    #[test]
    fn test_all_events_have_correct_schema_version() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "x", 0));
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::PaneOpened, None, json!({}));
        adapter.capture_control(1, RecorderControlMarkerType::Resize, json!({}));
        adapter.capture_ingress(
            1,
            "y".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::OperatorAction,
            None,
            RecorderEventCausality::default(),
        );

        for evt in sink.recorder_events() {
            assert_eq!(evt.schema_version, "ft.recorder.event.v1");
        }
    }

    // --- Causality ---

    #[test]
    fn test_egress_has_empty_causality_by_default() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "x", 0));

        let evt = &sink.recorder_events()[0];
        assert!(evt.causality.parent_event_id.is_none());
        assert!(evt.causality.trigger_event_id.is_none());
        assert!(evt.causality.root_event_id.is_none());
    }

    #[test]
    fn test_ingress_preserves_causality_chain() {
        let (sink, adapter) = make_adapter();
        adapter.capture_ingress(
            1,
            "cmd".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::WorkflowEngine,
            None,
            RecorderEventCausality {
                parent_event_id: Some("p1".into()),
                trigger_event_id: Some("t1".into()),
                root_event_id: Some("r1".into()),
            },
        );

        let evt = &sink.recorder_events()[0];
        assert_eq!(evt.causality.parent_event_id.as_deref(), Some("p1"));
        assert_eq!(evt.causality.trigger_event_id.as_deref(), Some("t1"));
        assert_eq!(evt.causality.root_event_id.as_deref(), Some("r1"));
    }

    // --- Empty content ---

    #[test]
    fn test_empty_text_egress_captured() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "", 0));
        assert_eq!(sink.len(), 1);
        match &sink.recorder_events()[0].payload {
            RecorderEventPayload::EgressOutput { text, .. } => {
                assert!(text.is_empty());
            }
            _ => panic!("expected EgressOutput"),
        }
    }

    #[test]
    fn test_empty_text_ingress_captured() {
        let (sink, adapter) = make_adapter();
        adapter.capture_ingress(
            1,
            String::new(),
            RecorderIngressKind::SendText,
            RecorderEventSource::OperatorAction,
            None,
            RecorderEventCausality::default(),
        );
        assert_eq!(sink.len(), 1);
    }

    // --- JSON serialization roundtrip ---

    #[test]
    fn test_captured_event_serializes_to_valid_json() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "hello", 0));

        let evt = &sink.recorder_events()[0];
        let json_str = serde_json::to_string(evt).unwrap();
        assert!(json_str.contains("egress_output"));
        assert!(json_str.contains("hello"));

        // Roundtrip
        let parsed: RecorderEvent = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed.event_id, evt.event_id);
        assert_eq!(parsed.pane_id, evt.pane_id);
    }

    #[test]
    fn test_lifecycle_event_serializes_to_valid_json() {
        let (sink, adapter) = make_adapter();
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::PaneOpened, None, json!({}));

        let evt = &sink.recorder_events()[0];
        let json_str = serde_json::to_string(evt).unwrap();
        assert!(json_str.contains("lifecycle_marker"));
        assert!(json_str.contains("pane_opened"));
    }

    #[test]
    fn test_control_event_serializes_to_valid_json() {
        let (sink, adapter) = make_adapter();
        adapter.capture_control(1, RecorderControlMarkerType::Resize, json!({"cols": 80}));

        let evt = &sink.recorder_events()[0];
        let json_str = serde_json::to_string(evt).unwrap();
        assert!(json_str.contains("control_marker"));
        assert!(json_str.contains("resize"));
    }

    // --- EgressTap implementation ---

    #[test]
    fn test_egress_tap_impl() {
        let sink = Arc::new(CollectingCaptureSink::new());
        let config = CaptureConfig::default();
        let adapter = CaptureAdapter::new(sink.clone(), config);

        let egress = EgressEvent {
            pane_id: 99,
            text: "tap test".into(),
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
            gap_reason: None,
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            occurred_at_ms: 1700000000000,
            sequence: 0,
            global_sequence: 0,
        };

        // Use the EgressTap trait
        EgressTap::on_egress(&adapter, egress);

        assert_eq!(sink.len(), 1);
        let evt = &sink.recorder_events()[0];
        assert_eq!(evt.pane_id, 99);
    }

    #[test]
    fn test_ingress_tap_impl_records_ingress_and_decision_marker() {
        let sink = Arc::new(CollectingCaptureSink::new());
        let config = CaptureConfig::default();
        let adapter = CaptureAdapter::new(sink.clone(), config);

        IngressTap::on_ingress(
            &adapter,
            IngressEvent {
                pane_id: 77,
                text: "rm -rf /".to_string(),
                source: RecorderEventSource::OperatorAction,
                ingress_kind: RecorderIngressKind::SendText,
                redaction: RecorderRedactionLevel::Partial,
                occurred_at_ms: 1700000000000,
                outcome: IngressOutcome::Denied {
                    reason: "policy deny".to_string(),
                },
                workflow_id: Some("wf-77".to_string()),
            },
        );

        let events = sink.recorder_events();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0].payload,
            RecorderEventPayload::IngressText { .. }
        ));
        assert!(matches!(
            events[1].payload,
            RecorderEventPayload::ControlMarker { .. }
        ));
    }

    // --- Negative timestamp ---

    #[test]
    fn test_negative_captured_at_uses_epoch_now() {
        let (sink, adapter) = make_adapter();
        let mut seg = make_segment(1, "neg", 0);
        seg.captured_at = -1;
        adapter.capture_egress(&seg);

        let evt = &sink.recorder_events()[0];
        // Should be a recent timestamp, not negative or zero
        assert!(evt.occurred_at_ms > 1600000000000);
    }

    // --- Mixed event types maintain independent sequences ---

    #[test]
    fn test_mixed_event_types_share_pane_sequence() {
        let (sink, adapter) = make_adapter();
        // All events for pane 1 share one sequence counter
        adapter.capture_egress(&make_segment(1, "e1", 0));
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::PaneOpened, None, json!({}));
        adapter.capture_control(1, RecorderControlMarkerType::Resize, json!({}));
        adapter.capture_ingress(
            1,
            "i1".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::OperatorAction,
            None,
            RecorderEventCausality::default(),
        );

        let events = sink.recorder_events();
        assert_eq!(events.len(), 4);
        // Sequences should be 0, 1, 2, 3
        for (i, evt) in events.iter().enumerate() {
            assert_eq!(evt.sequence, i as u64, "event {i} has wrong sequence");
        }
    }

    // --- Collecting sink operations ---

    #[test]
    fn test_collecting_sink_clear() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "x", 0));
        assert_eq!(sink.len(), 1);
        sink.clear();
        assert!(sink.is_empty());
    }

    #[test]
    fn test_collecting_sink_events_returns_clone() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "x", 0));
        let events1 = sink.events();
        let events2 = sink.events();
        assert_eq!(events1.len(), events2.len());
        assert_eq!(events1[0].0.event_id, events2[0].0.event_id);
    }

    // --- Merge key ordering ---

    #[test]
    fn test_merge_keys_sortable_across_panes() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(2, "pane2", 0));
        adapter.capture_egress(&make_segment(1, "pane1", 0));

        let events = sink.events();
        let mut keys: Vec<_> = events.iter().map(|(_, mk)| mk.clone()).collect();
        keys.sort();

        // After sorting, pane 1 should come before pane 2 (same timestamp)
        assert!(
            keys[0].pane_id <= keys[1].pane_id || keys[0].recorded_at_ms < keys[1].recorded_at_ms
        );
    }

    #[test]
    fn test_merge_key_stream_kind_ordering() {
        let (sink, adapter) = make_adapter();
        // Egress (rank 3) then Lifecycle (rank 0)
        adapter.capture_egress(&make_segment(1, "e", 0));
        adapter.capture_lifecycle(1, RecorderLifecyclePhase::PaneOpened, None, json!({}));

        let events = sink.events();
        let mut kinds: Vec<_> = events.iter().map(|(_, mk)| mk.stream_kind).collect();
        kinds.sort();

        // Lifecycle (rank 0) should sort before Egress (rank 3)
        assert_eq!(kinds[0], StreamKind::Lifecycle);
        assert_eq!(kinds[1], StreamKind::Egress);
    }

    // --- Default source ---

    #[test]
    fn test_default_source_is_wezterm_mux() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "x", 0));
        assert_eq!(
            sink.recorder_events()[0].source,
            RecorderEventSource::WeztermMux
        );
    }

    // --- parse roundtrip via parse_recorder_event_json ---

    #[test]
    fn test_captured_event_passes_schema_validation() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "validate me", 0));

        let evt = &sink.recorder_events()[0];
        let json_str = serde_json::to_string(evt).unwrap();
        let parsed = crate::recording::parse_recorder_event_json(&json_str);
        assert!(parsed.is_ok(), "schema validation failed: {parsed:?}");
    }

    #[test]
    fn test_ingress_event_passes_schema_validation() {
        let (sink, adapter) = make_adapter();
        adapter.capture_ingress(
            1,
            "cmd".into(),
            RecorderIngressKind::SendText,
            RecorderEventSource::OperatorAction,
            None,
            RecorderEventCausality::default(),
        );

        let evt = &sink.recorder_events()[0];
        let json_str = serde_json::to_string(evt).unwrap();
        let parsed = crate::recording::parse_recorder_event_json(&json_str);
        assert!(
            parsed.is_ok(),
            "ingress schema validation failed: {parsed:?}"
        );
    }

    // --- Unicode content ---

    #[test]
    fn test_unicode_content_captured_correctly() {
        let (sink, adapter) = make_adapter();
        adapter.capture_egress(&make_segment(1, "日本語テスト 🎉", 0));

        let evt = &sink.recorder_events()[0];
        match &evt.payload {
            RecorderEventPayload::EgressOutput { text, .. } => {
                assert_eq!(text, "日本語テスト 🎉");
            }
            _ => panic!("expected EgressOutput"),
        }
    }

    // --- Large content ---

    #[test]
    fn test_large_content_captured() {
        let (sink, adapter) = make_adapter();
        let large = "x".repeat(1_000_000);
        adapter.capture_egress(&make_segment(1, &large, 0));

        assert_eq!(sink.len(), 1);
        match &sink.recorder_events()[0].payload {
            RecorderEventPayload::EgressOutput { text, .. } => {
                assert_eq!(text.len(), 1_000_000);
            }
            _ => panic!("expected EgressOutput"),
        }
    }

    // --- Multiple panes interleaved ---

    #[test]
    fn test_interleaved_panes_all_captured() {
        let (sink, adapter) = make_adapter();
        for i in 0..10u64 {
            let pane_id = i % 3;
            adapter.capture_egress(&make_segment(pane_id, &format!("msg{i}"), i));
        }
        assert_eq!(sink.len(), 10);
    }
}
