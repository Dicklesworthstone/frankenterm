//! Deterministic event identity, merge ordering, and clock anomaly detection.
//!
//! Implements the `event_id.v1` algorithm from `docs/flight-recorder/sequence-correlation-model.md`:
//! - SHA-256 deterministic event IDs
//! - 5-key total merge ordering (RecorderMergeKey)
//! - Per-domain clock anomaly tracking
//!
//! Bead: wa-zn2u

use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::recording::{
    RecorderEvent, RecorderEventPayload, RecorderLifecyclePhase, RecorderSegmentKind,
};

/// Sequence stream domain for per-pane ordering.
///
/// Each pane maintains independent monotonic sequences per stream kind.
/// The rank ordering (lifecycle < control < ingress < egress) defines
/// deterministic tiebreak precedence when merging concurrent events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Lifecycle,
    Control,
    Ingress,
    Egress,
}

impl StreamKind {
    /// Returns the rank for deterministic ordering.
    /// Lower rank = higher priority in tiebreaks.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Lifecycle => 0,
            Self::Control => 1,
            Self::Ingress => 2,
            Self::Egress => 3,
        }
    }

    /// Derives the stream kind from a recorder event payload.
    #[must_use]
    pub fn from_payload(payload: &RecorderEventPayload) -> Self {
        match payload {
            RecorderEventPayload::LifecycleMarker { .. } => Self::Lifecycle,
            RecorderEventPayload::ControlMarker { .. } => Self::Control,
            RecorderEventPayload::IngressText { .. } => Self::Ingress,
            RecorderEventPayload::EgressOutput { .. } => Self::Egress,
        }
    }
}

impl PartialOrd for StreamKind {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for StreamKind {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.rank().cmp(&other.rank())
    }
}

/// Generate a deterministic event ID using the v1 algorithm.
///
/// Format: `sha256("{schema_version}|{pane_id}|{stream_kind_rank}|{sequence}|{event_type}|{occurred_at_ms}|{payload_hash}")`
///
/// Returns the hex-encoded SHA-256 digest (64 characters).
#[must_use]
pub fn generate_event_id_v1(event: &RecorderEvent) -> String {
    let stream_kind = StreamKind::from_payload(&event.payload);
    let event_type = event_type_str(&event.payload);
    let payload_hash = compute_payload_hash(&event.payload);

    let preimage = format!(
        "{}|{}|{}|{}|{}|{}|{}",
        event.schema_version,
        event.pane_id,
        stream_kind.rank(),
        event.sequence,
        event_type,
        event.occurred_at_ms,
        payload_hash,
    );

    let mut hasher = Sha256::new();
    hasher.update(preimage.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

/// Returns the serde tag string for a recorder event payload variant.
fn event_type_str(payload: &RecorderEventPayload) -> &'static str {
    match payload {
        RecorderEventPayload::IngressText { .. } => "ingress_text",
        RecorderEventPayload::EgressOutput { .. } => "egress_output",
        RecorderEventPayload::ControlMarker { .. } => "control_marker",
        RecorderEventPayload::LifecycleMarker { .. } => "lifecycle_marker",
    }
}

/// Computes a SHA-256 hash of the payload content for event_id derivation.
fn compute_payload_hash(payload: &RecorderEventPayload) -> String {
    let mut hasher = Sha256::new();
    match payload {
        RecorderEventPayload::IngressText { text, .. } => {
            hasher.update(b"ingress:");
            hasher.update(text.as_bytes());
        }
        RecorderEventPayload::EgressOutput { text, .. } => {
            hasher.update(b"egress:");
            hasher.update(text.as_bytes());
        }
        RecorderEventPayload::ControlMarker { details, .. } => {
            hasher.update(b"control:");
            hasher.update(details.to_string().as_bytes());
        }
        RecorderEventPayload::LifecycleMarker { details, .. } => {
            hasher.update(b"lifecycle:");
            hasher.update(details.to_string().as_bytes());
        }
    }
    hex::encode(hasher.finalize())
}

/// Composite key for deterministic total ordering of recorder events during merge.
///
/// Order: `recorded_at_ms` → `pane_id` → `stream_kind` rank → `sequence` → `event_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderMergeKey {
    pub recorded_at_ms: u64,
    pub pane_id: u64,
    pub stream_kind: StreamKind,
    pub sequence: u64,
    pub event_id: String,
}

impl PartialOrd for RecorderMergeKey {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for RecorderMergeKey {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.recorded_at_ms
            .cmp(&other.recorded_at_ms)
            .then_with(|| self.pane_id.cmp(&other.pane_id))
            .then_with(|| self.stream_kind.cmp(&other.stream_kind))
            .then_with(|| self.sequence.cmp(&other.sequence))
            .then_with(|| self.event_id.cmp(&other.event_id))
    }
}

impl RecorderMergeKey {
    /// Create a merge key from a recorder event.
    #[must_use]
    pub fn from_event(event: &RecorderEvent) -> Self {
        Self {
            recorded_at_ms: event.recorded_at_ms,
            pane_id: event.pane_id,
            stream_kind: StreamKind::from_payload(&event.payload),
            sequence: event.sequence,
            event_id: event.event_id.clone(),
        }
    }
}

/// Result of a clock anomaly check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClockAnomalyResult {
    /// True if the timestamp pair represents a clock anomaly.
    pub is_anomaly: bool,
    /// Human-readable reason if anomaly detected.
    pub reason: Option<String>,
}

/// Detect a clock anomaly between consecutive events.
///
/// An anomaly is detected when:
/// - `current < prev` (regression / backwards clock)
/// - `current > prev + threshold` when `threshold > 0` (suspicious forward skip)
#[must_use]
pub fn detect_clock_anomaly(
    current: u64,
    prev: u64,
    future_skew_threshold: u64,
) -> ClockAnomalyResult {
    if current < prev {
        return ClockAnomalyResult {
            is_anomaly: true,
            reason: Some(format!(
                "clock regression: current={} < prev={} (delta={}ms)",
                current,
                prev,
                prev - current
            )),
        };
    }
    if future_skew_threshold > 0 && current > prev + future_skew_threshold {
        return ClockAnomalyResult {
            is_anomaly: true,
            reason: Some(format!(
                "future skew: current={} > prev={} + threshold={} (delta={}ms)",
                current,
                prev,
                future_skew_threshold,
                current - prev
            )),
        };
    }
    ClockAnomalyResult {
        is_anomaly: false,
        reason: None,
    }
}

/// Tracks per-domain clock baselines and detects anomalies.
///
/// Each domain is identified by (pane_id, StreamKind).
pub struct ClockAnomalyTracker {
    last_timestamps: HashMap<(u64, StreamKind), u64>,
    future_skew_threshold_ms: u64,
}

impl ClockAnomalyTracker {
    /// Create a new tracker.
    ///
    /// Set `future_skew_threshold_ms` to 0 to disable forward-skew detection.
    #[must_use]
    pub fn new(future_skew_threshold_ms: u64) -> Self {
        Self {
            last_timestamps: HashMap::new(),
            future_skew_threshold_ms,
        }
    }

    /// Observe a timestamp for a (pane, stream) domain.
    ///
    /// Returns an anomaly result. Updates the baseline regardless
    /// (to recover from transient anomalies).
    pub fn observe(
        &mut self,
        pane_id: u64,
        stream_kind: StreamKind,
        timestamp_ms: u64,
    ) -> ClockAnomalyResult {
        let key = (pane_id, stream_kind);
        let result = if let Some(&prev) = self.last_timestamps.get(&key) {
            detect_clock_anomaly(timestamp_ms, prev, self.future_skew_threshold_ms)
        } else {
            ClockAnomalyResult {
                is_anomaly: false,
                reason: None,
            }
        };
        self.last_timestamps.insert(key, timestamp_ms);
        result
    }

    /// Returns the number of tracked domains.
    #[must_use]
    pub fn domain_count(&self) -> usize {
        self.last_timestamps.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RecorderControlMarkerType, RecorderEventCausality, RecorderEventSource,
        RecorderIngressKind, RecorderRedactionLevel, RecorderTextEncoding,
        RECORDER_EVENT_SCHEMA_VERSION_V1,
    };
    use serde_json::json;

    fn make_test_event(pane_id: u64, seq: u64, ts: u64, text: &str) -> RecorderEvent {
        RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: String::new(),
            pane_id,
            session_id: Some("s1".into()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: ts,
            recorded_at_ms: ts + 1,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.into(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        }
    }

    // -- StreamKind tests --

    #[test]
    fn stream_kind_rank_order() {
        assert!(StreamKind::Lifecycle.rank() < StreamKind::Control.rank());
        assert!(StreamKind::Control.rank() < StreamKind::Ingress.rank());
        assert!(StreamKind::Ingress.rank() < StreamKind::Egress.rank());
    }

    #[test]
    fn stream_kind_ord_matches_rank() {
        let mut kinds = vec![
            StreamKind::Egress,
            StreamKind::Lifecycle,
            StreamKind::Ingress,
            StreamKind::Control,
        ];
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                StreamKind::Lifecycle,
                StreamKind::Control,
                StreamKind::Ingress,
                StreamKind::Egress,
            ]
        );
    }

    #[test]
    fn stream_kind_from_payload_all_variants() {
        let ingress = RecorderEventPayload::IngressText {
            text: "x".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            ingress_kind: RecorderIngressKind::SendText,
        };
        assert_eq!(StreamKind::from_payload(&ingress), StreamKind::Ingress);

        let egress = RecorderEventPayload::EgressOutput {
            text: "y".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        };
        assert_eq!(StreamKind::from_payload(&egress), StreamKind::Egress);

        let control = RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::Resize,
            details: json!({}),
        };
        assert_eq!(StreamKind::from_payload(&control), StreamKind::Control);

        let lifecycle = RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: RecorderLifecyclePhase::CaptureStarted,
            reason: None,
            details: json!({}),
        };
        assert_eq!(StreamKind::from_payload(&lifecycle), StreamKind::Lifecycle);
    }

    #[test]
    fn stream_kind_serde_roundtrip() {
        let kinds = vec![
            StreamKind::Lifecycle,
            StreamKind::Control,
            StreamKind::Ingress,
            StreamKind::Egress,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let back: StreamKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, back);
        }
    }

    // -- event_id.v1 tests --

    #[test]
    fn event_id_v1_deterministic() {
        let e = make_test_event(1, 0, 1000, "hello");
        let id1 = generate_event_id_v1(&e);
        let id2 = generate_event_id_v1(&e);
        assert_eq!(id1, id2);
    }

    #[test]
    fn event_id_v1_is_64_hex_chars() {
        let e = make_test_event(1, 0, 1000, "hello");
        let id = generate_event_id_v1(&e);
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn event_id_v1_differs_by_pane() {
        let e1 = make_test_event(1, 0, 1000, "hello");
        let e2 = make_test_event(2, 0, 1000, "hello");
        assert_ne!(generate_event_id_v1(&e1), generate_event_id_v1(&e2));
    }

    #[test]
    fn event_id_v1_differs_by_sequence() {
        let e1 = make_test_event(1, 0, 1000, "hello");
        let e2 = make_test_event(1, 1, 1000, "hello");
        assert_ne!(generate_event_id_v1(&e1), generate_event_id_v1(&e2));
    }

    #[test]
    fn event_id_v1_differs_by_timestamp() {
        let e1 = make_test_event(1, 0, 1000, "hello");
        let e2 = make_test_event(1, 0, 2000, "hello");
        assert_ne!(generate_event_id_v1(&e1), generate_event_id_v1(&e2));
    }

    #[test]
    fn event_id_v1_differs_by_text() {
        let e1 = make_test_event(1, 0, 1000, "hello");
        let e2 = make_test_event(1, 0, 1000, "world");
        assert_ne!(generate_event_id_v1(&e1), generate_event_id_v1(&e2));
    }

    #[test]
    fn event_id_v1_differs_by_stream_kind() {
        let e1 = make_test_event(1, 0, 1000, "data");
        let mut e2 = e1.clone();
        e2.payload = RecorderEventPayload::EgressOutput {
            text: "data".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        };
        assert_ne!(generate_event_id_v1(&e1), generate_event_id_v1(&e2));
    }

    #[test]
    fn event_id_v1_all_payload_types() {
        let base = make_test_event(1, 0, 1000, "x");
        let ingress_id = generate_event_id_v1(&base);

        let mut egress = base.clone();
        egress.payload = RecorderEventPayload::EgressOutput {
            text: "y".into(),
            encoding: RecorderTextEncoding::Utf8,
            redaction: RecorderRedactionLevel::None,
            segment_kind: RecorderSegmentKind::Delta,
            is_gap: false,
        };
        let egress_id = generate_event_id_v1(&egress);

        let mut control = base.clone();
        control.payload = RecorderEventPayload::ControlMarker {
            control_marker_type: RecorderControlMarkerType::Resize,
            details: json!({"cols": 80}),
        };
        let control_id = generate_event_id_v1(&control);

        let mut lifecycle = base.clone();
        lifecycle.payload = RecorderEventPayload::LifecycleMarker {
            lifecycle_phase: RecorderLifecyclePhase::CaptureStarted,
            reason: None,
            details: json!({}),
        };
        let lifecycle_id = generate_event_id_v1(&lifecycle);

        let ids = vec![&ingress_id, &egress_id, &control_id, &lifecycle_id];
        for id in &ids {
            assert_eq!(id.len(), 64);
        }
        let mut deduped = ids.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), 4);
    }

    // -- RecorderMergeKey tests --

    #[test]
    fn merge_key_sorts_by_recorded_at() {
        let k1 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "a".into(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: 200,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "a".into(),
        };
        assert!(k1 < k2);
    }

    #[test]
    fn merge_key_tiebreak_pane_id() {
        let k1 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "a".into(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 2,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "a".into(),
        };
        assert!(k1 < k2);
    }

    #[test]
    fn merge_key_tiebreak_stream_kind() {
        let k1 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Lifecycle,
            sequence: 0,
            event_id: "a".into(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Egress,
            sequence: 0,
            event_id: "a".into(),
        };
        assert!(k1 < k2);
    }

    #[test]
    fn merge_key_tiebreak_sequence() {
        let k1 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "a".into(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 1,
            event_id: "a".into(),
        };
        assert!(k1 < k2);
    }

    #[test]
    fn merge_key_tiebreak_event_id() {
        let k1 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "aaa".into(),
        };
        let k2 = RecorderMergeKey {
            recorded_at_ms: 100,
            pane_id: 1,
            stream_kind: StreamKind::Ingress,
            sequence: 0,
            event_id: "bbb".into(),
        };
        assert!(k1 < k2);
    }

    #[test]
    fn merge_key_total_order_stability() {
        let mut keys = vec![
            RecorderMergeKey {
                recorded_at_ms: 100,
                pane_id: 2,
                stream_kind: StreamKind::Egress,
                sequence: 1,
                event_id: "z".into(),
            },
            RecorderMergeKey {
                recorded_at_ms: 100,
                pane_id: 1,
                stream_kind: StreamKind::Ingress,
                sequence: 0,
                event_id: "a".into(),
            },
            RecorderMergeKey {
                recorded_at_ms: 50,
                pane_id: 3,
                stream_kind: StreamKind::Lifecycle,
                sequence: 5,
                event_id: "m".into(),
            },
        ];
        let mut keys2 = keys.clone();
        keys.sort();
        keys2.sort();
        assert_eq!(keys, keys2);
        assert_eq!(keys[0].recorded_at_ms, 50);
        assert_eq!(keys[1].pane_id, 1);
        assert_eq!(keys[2].pane_id, 2);
    }

    #[test]
    fn merge_key_from_event() {
        let e = make_test_event(42, 7, 5000, "cmd");
        let key = RecorderMergeKey::from_event(&e);
        assert_eq!(key.recorded_at_ms, 5001);
        assert_eq!(key.pane_id, 42);
        assert_eq!(key.stream_kind, StreamKind::Ingress);
        assert_eq!(key.sequence, 7);
    }

    // -- Clock anomaly detection tests --

    #[test]
    fn clock_anomaly_no_anomaly() {
        let r = detect_clock_anomaly(200, 100, 0);
        assert!(!r.is_anomaly);
        assert!(r.reason.is_none());
    }

    #[test]
    fn clock_anomaly_regression() {
        let r = detect_clock_anomaly(50, 100, 0);
        assert!(r.is_anomaly);
        assert!(r.reason.as_ref().unwrap().contains("regression"));
    }

    #[test]
    fn clock_anomaly_equal_timestamps_ok() {
        let r = detect_clock_anomaly(100, 100, 0);
        assert!(!r.is_anomaly);
    }

    #[test]
    fn clock_anomaly_future_skew() {
        let r = detect_clock_anomaly(10_000, 100, 5_000);
        assert!(r.is_anomaly);
        assert!(r.reason.as_ref().unwrap().contains("future skew"));
    }

    #[test]
    fn clock_anomaly_threshold_disabled() {
        let r = detect_clock_anomaly(999_999, 100, 0);
        assert!(!r.is_anomaly);
    }

    // -- ClockAnomalyTracker tests --

    #[test]
    fn tracker_first_event_no_anomaly() {
        let mut t = ClockAnomalyTracker::new(0);
        let r = t.observe(1, StreamKind::Ingress, 1000);
        assert!(!r.is_anomaly);
    }

    #[test]
    fn tracker_regression_detected() {
        let mut t = ClockAnomalyTracker::new(0);
        t.observe(1, StreamKind::Ingress, 1000);
        let r = t.observe(1, StreamKind::Ingress, 500);
        assert!(r.is_anomaly);
    }

    #[test]
    fn tracker_independent_domains_by_pane() {
        let mut t = ClockAnomalyTracker::new(0);
        t.observe(1, StreamKind::Ingress, 1000);
        let r = t.observe(2, StreamKind::Ingress, 500);
        assert!(!r.is_anomaly);
    }

    #[test]
    fn tracker_independent_domains_by_stream() {
        let mut t = ClockAnomalyTracker::new(0);
        t.observe(1, StreamKind::Ingress, 1000);
        let r = t.observe(1, StreamKind::Egress, 500);
        assert!(!r.is_anomaly);
    }

    #[test]
    fn tracker_domain_count() {
        let mut t = ClockAnomalyTracker::new(0);
        assert_eq!(t.domain_count(), 0);
        t.observe(1, StreamKind::Ingress, 100);
        assert_eq!(t.domain_count(), 1);
        t.observe(1, StreamKind::Egress, 100);
        assert_eq!(t.domain_count(), 2);
        t.observe(2, StreamKind::Ingress, 100);
        assert_eq!(t.domain_count(), 3);
        t.observe(1, StreamKind::Ingress, 200);
        assert_eq!(t.domain_count(), 3);
    }

    #[test]
    fn tracker_future_skew_detected() {
        let mut t = ClockAnomalyTracker::new(5_000);
        t.observe(1, StreamKind::Ingress, 1000);
        let r = t.observe(1, StreamKind::Ingress, 10_000);
        assert!(r.is_anomaly);
        assert!(r.reason.as_ref().unwrap().contains("future skew"));
    }

    #[test]
    fn tracker_baseline_updates_after_anomaly() {
        let mut t = ClockAnomalyTracker::new(0);
        t.observe(1, StreamKind::Ingress, 1000);
        let r = t.observe(1, StreamKind::Ingress, 500);
        assert!(r.is_anomaly);
        let r2 = t.observe(1, StreamKind::Ingress, 600);
        assert!(!r2.is_anomaly);
    }
}
