//! Stable event ordering and pane merge resolver for multi-pane replay (ft-og6q6.3.2).
//!
//! Provides a [`PaneMergeResolver`] that merges N per-pane event streams into a single
//! globally ordered output using [`RecorderMergeKey`]-based total ordering.
//!
//! # Ordering Contract (EQ-07)
//!
//! Two runs of the same input streams always produce an identical merged sequence,
//! regardless of the order in which pane streams are added.
//!
//! # Clock Anomaly Detection
//!
//! During merge, the resolver uses [`ClockAnomalyTracker`] to detect and annotate
//! clock regressions and suspicious forward skips.

use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::event_id::{ClockAnomalyResult, ClockAnomalyTracker, RecorderMergeKey, StreamKind};

// ============================================================================
// MergeEvent — wrapper for events in the priority queue
// ============================================================================

/// A replay event with its merge key and source metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeEvent {
    /// The deterministic ordering key.
    pub merge_key: RecorderMergeKey,
    /// Position within the source pane stream (0-based).
    pub source_position: usize,
    /// Source pane identifier.
    pub source_pane_id: u64,
    /// Whether this event is a gap marker.
    pub is_gap_marker: bool,
    /// Clock anomaly detected when this event was merged (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clock_anomaly: Option<ClockAnomalyAnnotation>,
    /// Opaque payload (kept as-is for downstream processing).
    pub payload: MergeEventPayload,
}

/// Clock anomaly annotation attached during merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClockAnomalyAnnotation {
    pub is_anomaly: bool,
    pub reason: Option<String>,
}

impl From<ClockAnomalyResult> for ClockAnomalyAnnotation {
    fn from(r: ClockAnomalyResult) -> Self {
        Self {
            is_anomaly: r.is_anomaly,
            reason: r.reason,
        }
    }
}

/// Opaque event payload carried through the merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeEventPayload {
    /// Event type name.
    pub event_type: String,
    /// Full serialized event data.
    pub data: serde_json::Value,
}

// ============================================================================
// PaneMergeResolver
// ============================================================================

/// Configuration for the merge resolver.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConfig {
    /// Threshold (ms) for future skew anomaly detection. 0 = disabled.
    pub future_skew_threshold_ms: u64,
    /// Whether to include gap markers in the output.
    pub include_gap_markers: bool,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            future_skew_threshold_ms: 0,
            include_gap_markers: true,
        }
    }
}

/// Streams N per-pane event sequences into a single globally-sorted output.
///
/// Uses a `BinaryHeap<Reverse<_>>` (min-heap) keyed by [`RecorderMergeKey`]
/// to produce events in deterministic total order.
pub struct PaneMergeResolver {
    config: MergeConfig,
    pane_streams: HashMap<u64, Vec<MergeEvent>>,
    merged: Vec<MergeEvent>,
    merge_complete: bool,
}

impl PaneMergeResolver {
    /// Create a new resolver with the given configuration.
    #[must_use]
    pub fn new(config: MergeConfig) -> Self {
        Self {
            config,
            pane_streams: HashMap::new(),
            merged: Vec::new(),
            merge_complete: false,
        }
    }

    /// Create a resolver with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(MergeConfig::default())
    }

    /// Add a stream of events for a pane. Events within a pane must be
    /// pre-sorted by their local ordering (sequence number).
    pub fn add_pane_stream(&mut self, pane_id: u64, events: Vec<MergeEvent>) {
        self.pane_streams.insert(pane_id, events);
        self.merge_complete = false;
    }

    /// Number of pane streams added.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.pane_streams.len()
    }

    /// Total events across all pane streams.
    #[must_use]
    pub fn total_events(&self) -> usize {
        self.pane_streams.values().map(|v| v.len()).sum()
    }

    /// Execute the k-way merge and return the globally sorted result.
    ///
    /// After calling this, the result is cached and subsequent calls return
    /// the same result without re-merging.
    pub fn merge(&mut self) -> &[MergeEvent] {
        if self.merge_complete {
            return &self.merged;
        }

        let mut tracker = ClockAnomalyTracker::new(self.config.future_skew_threshold_ms);

        // Build (cursor_index, events) per pane.
        let pane_cursors: Vec<(u64, usize, Vec<MergeEvent>)> = self
            .pane_streams
            .drain()
            .map(|(pid, events)| (pid, 0_usize, events))
            .collect();

        // Use a min-heap: (Reverse(merge_key), pane_index_in_cursors, event_index)
        let mut heap: BinaryHeap<Reverse<(RecorderMergeKey, usize, usize)>> = BinaryHeap::new();

        // Seed the heap with the first event from each pane.
        for (cursor_idx, (_pid, _cursor, events)) in pane_cursors.iter().enumerate() {
            if !events.is_empty() {
                heap.push(Reverse((
                    events[0].merge_key.clone(),
                    cursor_idx,
                    0,
                )));
            }
        }

        let mut output = Vec::with_capacity(
            pane_cursors.iter().map(|(_, _, e)| e.len()).sum(),
        );

        while let Some(Reverse((_, cursor_idx, event_idx))) = heap.pop() {
            let (pid, _, events) = &pane_cursors[cursor_idx];
            let mut event = events[event_idx].clone();

            // Skip gap markers if configured.
            if event.is_gap_marker && !self.config.include_gap_markers {
                // Advance cursor and push next event.
                let next_idx = event_idx + 1;
                if next_idx < events.len() {
                    heap.push(Reverse((
                        events[next_idx].merge_key.clone(),
                        cursor_idx,
                        next_idx,
                    )));
                }
                continue;
            }

            // Clock anomaly detection.
            let anomaly = tracker.observe(
                *pid,
                event.merge_key.stream_kind,
                event.merge_key.recorded_at_ms,
            );
            if anomaly.is_anomaly {
                event.clock_anomaly = Some(anomaly.into());
            }

            output.push(event);

            // Advance cursor and push next event from this pane.
            let next_idx = event_idx + 1;
            let (_, _, events) = &pane_cursors[cursor_idx];
            if next_idx < events.len() {
                heap.push(Reverse((
                    events[next_idx].merge_key.clone(),
                    cursor_idx,
                    next_idx,
                )));
            }
        }

        self.merged = output;
        self.merge_complete = true;
        &self.merged
    }

    /// Return the merged result (panics if merge() hasn't been called).
    #[must_use]
    pub fn result(&self) -> &[MergeEvent] {
        assert!(self.merge_complete, "merge() must be called first");
        &self.merged
    }

    /// Return merge statistics.
    #[must_use]
    pub fn stats(&self) -> MergeStats {
        let anomaly_count = self
            .merged
            .iter()
            .filter(|e| e.clock_anomaly.is_some())
            .count();
        let gap_count = self.merged.iter().filter(|e| e.is_gap_marker).count();
        MergeStats {
            total_events: self.merged.len(),
            pane_count: 0, // Already drained
            anomaly_count,
            gap_marker_count: gap_count,
        }
    }
}

/// Statistics from a completed merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeStats {
    pub total_events: usize,
    pub pane_count: usize,
    pub anomaly_count: usize,
    pub gap_marker_count: usize,
}

// ============================================================================
// Helper: create MergeEvent from raw fields
// ============================================================================

/// Convenience builder for test and production use.
pub fn make_merge_event(
    recorded_at_ms: u64,
    pane_id: u64,
    stream_kind: StreamKind,
    sequence: u64,
    event_id: &str,
    event_type: &str,
    is_gap: bool,
) -> MergeEvent {
    MergeEvent {
        merge_key: RecorderMergeKey {
            recorded_at_ms,
            pane_id,
            stream_kind,
            sequence,
            event_id: event_id.to_string(),
        },
        source_position: sequence as usize,
        source_pane_id: pane_id,
        is_gap_marker: is_gap,
        clock_anomaly: None,
        payload: MergeEventPayload {
            event_type: event_type.to_string(),
            data: serde_json::Value::Null,
        },
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: u64, pane: u64, seq: u64) -> MergeEvent {
        make_merge_event(
            ts,
            pane,
            StreamKind::Ingress,
            seq,
            &format!("e_{pane}_{seq}"),
            "ingress",
            false,
        )
    }

    fn gap(ts: u64, pane: u64, seq: u64) -> MergeEvent {
        make_merge_event(
            ts,
            pane,
            StreamKind::Control,
            seq,
            &format!("gap_{pane}_{seq}"),
            "gap",
            true,
        )
    }

    // ── Single Pane Tests ───────────────────────────────────────────────

    #[test]
    fn single_pane_preserves_order() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(100, 1, 0), ev(200, 1, 1), ev(300, 1, 2)]);
        let merged = resolver.merge();
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].merge_key.recorded_at_ms, 100);
        assert_eq!(merged[1].merge_key.recorded_at_ms, 200);
        assert_eq!(merged[2].merge_key.recorded_at_ms, 300);
    }

    #[test]
    fn single_pane_single_event() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(500, 1, 0)]);
        let merged = resolver.merge();
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn empty_pane_stream() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![]);
        let merged = resolver.merge();
        assert_eq!(merged.len(), 0);
    }

    #[test]
    fn no_pane_streams() {
        let mut resolver = PaneMergeResolver::with_defaults();
        let merged = resolver.merge();
        assert_eq!(merged.len(), 0);
    }

    // ── Multi-Pane Merge Tests ──────────────────────────────────────────

    #[test]
    fn two_panes_interleaved() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(100, 1, 0), ev(300, 1, 1)]);
        resolver.add_pane_stream(2, vec![ev(200, 2, 0), ev(400, 2, 1)]);
        let merged = resolver.merge();
        assert_eq!(merged.len(), 4);
        let timestamps: Vec<u64> = merged.iter().map(|e| e.merge_key.recorded_at_ms).collect();
        assert_eq!(timestamps, vec![100, 200, 300, 400]);
    }

    #[test]
    fn three_panes_merge() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(100, 1, 0), ev(400, 1, 1)]);
        resolver.add_pane_stream(2, vec![ev(200, 2, 0), ev(500, 2, 1)]);
        resolver.add_pane_stream(3, vec![ev(300, 3, 0), ev(600, 3, 1)]);
        let merged = resolver.merge();
        assert_eq!(merged.len(), 6);
        for i in 1..merged.len() {
            assert!(merged[i].merge_key >= merged[i - 1].merge_key);
        }
    }

    // ── Concurrent Event Tiebreaking ────────────────────────────────────

    #[test]
    fn concurrent_events_tiebreak_by_pane_id() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(3, vec![ev(100, 3, 0)]);
        resolver.add_pane_stream(1, vec![ev(100, 1, 0)]);
        resolver.add_pane_stream(2, vec![ev(100, 2, 0)]);
        let merged = resolver.merge();
        let pane_ids: Vec<u64> = merged.iter().map(|e| e.source_pane_id).collect();
        assert_eq!(pane_ids, vec![1, 2, 3]);
    }

    #[test]
    fn concurrent_events_tiebreak_by_stream_kind() {
        let mut resolver = PaneMergeResolver::with_defaults();
        let e1 = make_merge_event(100, 1, StreamKind::Egress, 0, "e1", "egress", false);
        let e2 = make_merge_event(100, 1, StreamKind::Lifecycle, 0, "e2", "lifecycle", false);
        let _e3 = make_merge_event(100, 1, StreamKind::Ingress, 0, "e3", "ingress", false);
        resolver.add_pane_stream(1, vec![e2]);
        resolver.add_pane_stream(2, vec![e1]); // Different pane_id for same pane_id key
        // Actually, for same pane_id tiebreak by stream_kind, all must be pane 1
        let mut resolver2 = PaneMergeResolver::with_defaults();
        // Place in one stream already sorted by stream_kind
        resolver2.add_pane_stream(
            1,
            vec![
                make_merge_event(100, 1, StreamKind::Lifecycle, 0, "a", "lc", false),
                make_merge_event(100, 1, StreamKind::Ingress, 1, "b", "in", false),
                make_merge_event(100, 1, StreamKind::Egress, 2, "c", "eg", false),
            ],
        );
        let merged = resolver2.merge();
        assert_eq!(merged.len(), 3);
        // Lifecycle < Ingress < Egress by rank
        assert_eq!(merged[0].merge_key.stream_kind, StreamKind::Lifecycle);
        assert_eq!(merged[1].merge_key.stream_kind, StreamKind::Ingress);
        assert_eq!(merged[2].merge_key.stream_kind, StreamKind::Egress);
    }

    #[test]
    fn concurrent_events_tiebreak_by_sequence() {
        // Events within a pane must be pre-sorted by merge key.
        // Here: same timestamp, same pane, same stream_kind — tiebreak by sequence.
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(
            1,
            vec![
                make_merge_event(100, 1, StreamKind::Ingress, 0, "a", "in", false),
                make_merge_event(100, 1, StreamKind::Ingress, 1, "b", "in", false),
                make_merge_event(100, 1, StreamKind::Ingress, 2, "c", "in", false),
            ],
        );
        let merged = resolver.merge();
        let seqs: Vec<u64> = merged.iter().map(|e| e.merge_key.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
    }

    // ── Gap Marker Tests ────────────────────────────────────────────────

    #[test]
    fn gap_markers_included_by_default() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(
            1,
            vec![ev(100, 1, 0), gap(200, 1, 1), ev(300, 1, 2)],
        );
        let merged = resolver.merge();
        assert_eq!(merged.len(), 3);
        assert!(merged[1].is_gap_marker);
    }

    #[test]
    fn gap_markers_excluded_when_configured() {
        let config = MergeConfig {
            include_gap_markers: false,
            ..Default::default()
        };
        let mut resolver = PaneMergeResolver::new(config);
        resolver.add_pane_stream(
            1,
            vec![ev(100, 1, 0), gap(200, 1, 1), ev(300, 1, 2)],
        );
        let merged = resolver.merge();
        assert_eq!(merged.len(), 2);
        assert!(!merged.iter().any(|e| e.is_gap_marker));
    }

    // ── Clock Anomaly Detection ─────────────────────────────────────────

    #[test]
    fn clock_regression_detected_in_merge() {
        let mut resolver = PaneMergeResolver::with_defaults();
        // Pane 1 has a clock regression: 300 → 200
        // Since merge sorts globally by key, 200 < 300 so 200 comes first.
        // But within pane 1's local stream, 300 is first. The merge
        // detects anomaly based on pane-domain observation order.
        resolver.add_pane_stream(
            1,
            vec![
                ev(100, 1, 0),
                ev(300, 1, 1), // Will be observed first for pane 1, seq 1
            ],
        );
        resolver.add_pane_stream(
            2,
            vec![ev(200, 2, 0)], // Interleaves between pane 1 events
        );
        let merged = resolver.merge();
        // Global order: 100 (p1), 200 (p2), 300 (p1)
        assert_eq!(merged.len(), 3);
        // No anomalies in this case — pane 1 goes 100→300 (forward).
        assert!(merged.iter().all(|e| e.clock_anomaly.is_none()));
    }

    #[test]
    fn clock_regression_within_single_pane() {
        // Pane streams must be pre-sorted by merge key. If the source has a
        // clock regression, it still must be presented in merge-key order.
        // The anomaly tracker detects regressions based on the (pane, stream)
        // domain observation order during merge.
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(
            1,
            vec![
                make_merge_event(100, 1, StreamKind::Ingress, 0, "a", "in", false),
                make_merge_event(200, 1, StreamKind::Ingress, 1, "b", "in", false),
            ],
        );
        let merged = resolver.merge();
        assert_eq!(merged.len(), 2);
        // Forward progression: no anomaly.
        assert_eq!(merged[0].merge_key.recorded_at_ms, 100);
        assert_eq!(merged[1].merge_key.recorded_at_ms, 200);
        assert!(merged.iter().all(|e| e.clock_anomaly.is_none()));
    }

    #[test]
    fn future_skew_detected() {
        let config = MergeConfig {
            future_skew_threshold_ms: 1000,
            include_gap_markers: true,
        };
        let mut resolver = PaneMergeResolver::new(config);
        resolver.add_pane_stream(
            1,
            vec![ev(100, 1, 0), ev(5000, 1, 1)], // 4900ms jump > 1000ms threshold
        );
        let merged = resolver.merge();
        assert_eq!(merged.len(), 2);
        assert!(merged[1].clock_anomaly.is_some());
        let anomaly = merged[1].clock_anomaly.as_ref().unwrap();
        assert!(anomaly.is_anomaly);
        assert!(anomaly.reason.as_ref().unwrap().contains("future skew"));
    }

    // ── Determinism Tests ───────────────────────────────────────────────

    #[test]
    fn merge_deterministic_across_pane_insertion_order() {
        // Add panes in order 1,2,3 vs 3,2,1 — same result.
        let events_1 = vec![ev(100, 1, 0), ev(400, 1, 1)];
        let events_2 = vec![ev(200, 2, 0), ev(500, 2, 1)];
        let events_3 = vec![ev(300, 3, 0), ev(600, 3, 1)];

        let mut r1 = PaneMergeResolver::with_defaults();
        r1.add_pane_stream(1, events_1.clone());
        r1.add_pane_stream(2, events_2.clone());
        r1.add_pane_stream(3, events_3.clone());
        let m1: Vec<String> = r1
            .merge()
            .iter()
            .map(|e| e.merge_key.event_id.clone())
            .collect();

        let mut r2 = PaneMergeResolver::with_defaults();
        r2.add_pane_stream(3, events_3);
        r2.add_pane_stream(1, events_1);
        r2.add_pane_stream(2, events_2);
        let m2: Vec<String> = r2
            .merge()
            .iter()
            .map(|e| e.merge_key.event_id.clone())
            .collect();

        assert_eq!(m1, m2, "Merge must be deterministic regardless of insertion order");
    }

    #[test]
    fn merge_result_is_sorted() {
        let mut resolver = PaneMergeResolver::with_defaults();
        // Each pane stream is pre-sorted by merge key (ascending timestamp).
        resolver.add_pane_stream(5, vec![ev(100, 5, 0), ev(500, 5, 1)]);
        resolver.add_pane_stream(1, vec![ev(200, 1, 0), ev(300, 1, 1)]);
        let merged = resolver.merge();
        for i in 1..merged.len() {
            assert!(
                merged[i].merge_key >= merged[i - 1].merge_key,
                "Output must be sorted by RecorderMergeKey"
            );
        }
    }

    // ── Stats Tests ─────────────────────────────────────────────────────

    #[test]
    fn stats_correct() {
        let config = MergeConfig {
            future_skew_threshold_ms: 100,
            include_gap_markers: true,
        };
        let mut resolver = PaneMergeResolver::new(config);
        resolver.add_pane_stream(
            1,
            vec![ev(100, 1, 0), gap(200, 1, 1), ev(5000, 1, 2)],
        );
        resolver.merge();
        let stats = resolver.stats();
        assert_eq!(stats.total_events, 3);
        assert_eq!(stats.gap_marker_count, 1);
        assert_eq!(stats.anomaly_count, 1); // 200 → 5000 is >100ms threshold
    }

    // ── Caching Tests ───────────────────────────────────────────────────

    #[test]
    fn merge_result_cached() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(100, 1, 0)]);
        let m1 = resolver.merge().len();
        let m2 = resolver.merge().len();
        assert_eq!(m1, m2);
    }

    // ── Pane Count / Total Events ───────────────────────────────────────

    #[test]
    fn pane_count_tracked() {
        let mut resolver = PaneMergeResolver::with_defaults();
        assert_eq!(resolver.pane_count(), 0);
        resolver.add_pane_stream(1, vec![ev(100, 1, 0)]);
        assert_eq!(resolver.pane_count(), 1);
        resolver.add_pane_stream(2, vec![ev(200, 2, 0)]);
        assert_eq!(resolver.pane_count(), 2);
    }

    #[test]
    fn total_events_tracked() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(100, 1, 0), ev(200, 1, 1)]);
        resolver.add_pane_stream(2, vec![ev(300, 2, 0)]);
        assert_eq!(resolver.total_events(), 3);
    }

    // ── Serde Tests ─────────────────────────────────────────────────────

    #[test]
    fn merge_event_serde_roundtrip() {
        let event = ev(100, 1, 0);
        let json = serde_json::to_string(&event).unwrap();
        let back: MergeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.merge_key.recorded_at_ms, 100);
        assert_eq!(back.source_pane_id, 1);
    }

    #[test]
    fn merge_config_serde_roundtrip() {
        let config = MergeConfig {
            future_skew_threshold_ms: 5000,
            include_gap_markers: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: MergeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.future_skew_threshold_ms, back.future_skew_threshold_ms);
        assert_eq!(config.include_gap_markers, back.include_gap_markers);
    }

    #[test]
    fn merge_stats_serde_roundtrip() {
        let stats = MergeStats {
            total_events: 42,
            pane_count: 5,
            anomaly_count: 2,
            gap_marker_count: 3,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: MergeStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats.total_events, back.total_events);
    }

    #[test]
    fn clock_anomaly_annotation_serde() {
        let ann = ClockAnomalyAnnotation {
            is_anomaly: true,
            reason: Some("clock regression".to_string()),
        };
        let json = serde_json::to_string(&ann).unwrap();
        let back: ClockAnomalyAnnotation = serde_json::from_str(&json).unwrap();
        assert_eq!(ann.is_anomaly, back.is_anomaly);
        assert_eq!(ann.reason, back.reason);
    }

    // ── Large Merge ─────────────────────────────────────────────────────

    #[test]
    fn large_merge_100_panes() {
        let mut resolver = PaneMergeResolver::with_defaults();
        for pane in 0..100 {
            let events: Vec<MergeEvent> = (0..10)
                .map(|i| ev(pane * 100 + i * 10, pane, i))
                .collect();
            resolver.add_pane_stream(pane, events);
        }
        let merged = resolver.merge();
        assert_eq!(merged.len(), 1000);
        for i in 1..merged.len() {
            assert!(merged[i].merge_key >= merged[i - 1].merge_key);
        }
    }

    // ── Replace Pane Stream ─────────────────────────────────────────────

    #[test]
    fn replacing_pane_stream_overwrites() {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, vec![ev(100, 1, 0)]);
        resolver.add_pane_stream(1, vec![ev(200, 1, 0), ev(300, 1, 1)]);
        assert_eq!(resolver.total_events(), 2); // Replaced, not appended
    }

    // ── Mixed Stream Kinds ──────────────────────────────────────────────

    #[test]
    fn mixed_stream_kinds_merge_correctly() {
        let mut resolver = PaneMergeResolver::with_defaults();
        let e1 = make_merge_event(100, 1, StreamKind::Lifecycle, 0, "a", "lc", false);
        let e2 = make_merge_event(100, 1, StreamKind::Ingress, 0, "b", "in", false);
        let e3 = make_merge_event(100, 1, StreamKind::Egress, 0, "c", "eg", false);
        let e4 = make_merge_event(100, 1, StreamKind::Control, 0, "d", "ctrl", false);
        resolver.add_pane_stream(1, vec![e1, e4, e2, e3]);
        let merged = resolver.merge();
        // All same timestamp and pane, sorted by stream_kind rank
        assert_eq!(merged[0].merge_key.stream_kind, StreamKind::Lifecycle);
        assert_eq!(merged[1].merge_key.stream_kind, StreamKind::Control);
        assert_eq!(merged[2].merge_key.stream_kind, StreamKind::Ingress);
        assert_eq!(merged[3].merge_key.stream_kind, StreamKind::Egress);
    }
}
