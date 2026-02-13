//! Deterministic sequence assignment and correlation model for replayable causality.
//!
//! This module defines the ordering and correlation rules that make flight-recorder
//! replay deterministic across panes. It bridges the per-pane sequence counters
//! (from [`IngressSequence`] / [`EgressEvent::sequence`]) with the process-wide
//! [`GlobalSequence`] and adds correlation metadata for causal reconstruction.
//!
//! # Ordering Contract
//!
//! Events are replay-ordered by `(global_sequence, pane_id, sequence)` — a
//! lexicographic triple that is:
//! - **Total**: every event has a unique position.
//! - **Stable**: repeated sorts produce identical order.
//! - **Deterministic**: independent of wall-clock skew between panes.
//!
//! Within a single pane, `sequence` is strictly monotonic. Across panes,
//! `global_sequence` provides a total interleaving that reflects the order
//! events were *observed* by the recorder process, not the order they
//! occurred in the terminal (which may involve non-trivial latency).
//!
//! # Clock Skew Handling
//!
//! `occurred_at_ms` reflects the producer's wall clock and may be non-monotonic
//! or skewed relative to other panes. The sequence model treats timestamps as
//! *advisory metadata* — ordering for replay uses sequence numbers exclusively.
//! A [`ClockSkewDetector`] flags anomalies for diagnostic purposes without
//! altering the replay order.
//!
//! # Correlation
//!
//! Every event carries a [`CorrelationContext`] that links it to:
//! - Its pane-local predecessor (`parent_event_id`)
//! - The triggering action that caused it (`trigger_event_id`)
//! - The root of the causal chain (`root_event_id`)
//! - An optional batch ID grouping atomically-related events
//!
//! # Example
//!
//! ```
//! use frankenterm_core::sequence_model::{SequenceAssigner, ReplayOrder, ClockSkewPolicy};
//!
//! let assigner = SequenceAssigner::new();
//!
//! // Assign sequences for pane 0
//! let (pane_seq, global_seq) = assigner.assign(0);
//! assert_eq!(pane_seq, 0);
//! assert_eq!(global_seq, 0);
//!
//! // Assign for pane 1 — global advances, pane starts fresh
//! let (pane_seq, global_seq) = assigner.assign(1);
//! assert_eq!(pane_seq, 0);
//! assert_eq!(global_seq, 1);
//!
//! // Assign again for pane 0 — both advance
//! let (pane_seq, global_seq) = assigner.assign(0);
//! assert_eq!(pane_seq, 1);
//! assert_eq!(global_seq, 2);
//! ```

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Sequence assignment
// ---------------------------------------------------------------------------

/// Assigns per-pane and global sequence numbers atomically.
///
/// Thread-safe: the global counter uses `AtomicU64` (lock-free) and per-pane
/// counters are behind a `Mutex<HashMap>` that is held only long enough to
/// fetch-and-increment.
#[derive(Debug)]
pub struct SequenceAssigner {
    /// Process-wide monotonic counter.
    global: AtomicU64,
    /// Per-pane monotonic counters.
    pane_sequences: Mutex<HashMap<u64, u64>>,
}

impl SequenceAssigner {
    /// Create a new assigner with all counters at 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            global: AtomicU64::new(0),
            pane_sequences: Mutex::new(HashMap::new()),
        }
    }

    /// Assign the next `(pane_sequence, global_sequence)` pair for `pane_id`.
    ///
    /// Both counters are strictly monotonic. The global sequence provides a
    /// total ordering across all panes; the pane sequence orders events within
    /// a single pane.
    pub fn assign(&self, pane_id: u64) -> (u64, u64) {
        let pane_seq = {
            let mut map = self.pane_sequences.lock().unwrap();
            let entry = map.entry(pane_id).or_insert(0);
            let seq = *entry;
            *entry += 1;
            seq
        };
        let global_seq = self.global.fetch_add(1, Ordering::Relaxed);
        (pane_seq, global_seq)
    }

    /// Return the current global sequence value without advancing it.
    pub fn current_global(&self) -> u64 {
        self.global.load(Ordering::Relaxed)
    }

    /// Return the current per-pane sequence for a given pane (0 if unseen).
    pub fn current_pane(&self, pane_id: u64) -> u64 {
        self.pane_sequences
            .lock()
            .unwrap()
            .get(&pane_id)
            .copied()
            .unwrap_or(0)
    }

    /// Return the number of distinct panes that have been assigned sequences.
    pub fn pane_count(&self) -> usize {
        self.pane_sequences.lock().unwrap().len()
    }

    /// Reset per-pane counter for a specific pane (e.g., pane closed and reopened).
    ///
    /// The global counter is never reset — it is process-lifetime monotonic.
    pub fn reset_pane(&self, pane_id: u64) {
        self.pane_sequences.lock().unwrap().remove(&pane_id);
    }
}

impl Default for SequenceAssigner {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Replay ordering
// ---------------------------------------------------------------------------

/// A replay-ordering key for deterministic event replay.
///
/// Events are sorted by this key to produce a stable, total order
/// regardless of wall-clock timestamps.
///
/// Ordering: `global_sequence` → `pane_id` → `sequence` (lexicographic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReplayOrder {
    /// Process-wide monotonic sequence (primary sort key).
    pub global_sequence: u64,
    /// Pane identifier (tiebreaker for events assigned in the same batch).
    pub pane_id: u64,
    /// Per-pane monotonic sequence (final tiebreaker).
    pub sequence: u64,
}

impl ReplayOrder {
    /// Create a new replay ordering key.
    #[must_use]
    pub fn new(global_sequence: u64, pane_id: u64, sequence: u64) -> Self {
        Self {
            global_sequence,
            pane_id,
            sequence,
        }
    }

    /// Returns true if `self` happened before `other` in replay order.
    pub fn is_before(&self, other: &Self) -> bool {
        self < other
    }

    /// Returns true if two events are concurrent (same global sequence but
    /// different panes). In practice this shouldn't happen with the current
    /// single-threaded assigner, but the model supports it.
    pub fn is_concurrent_with(&self, other: &Self) -> bool {
        self.global_sequence == other.global_sequence && self.pane_id != other.pane_id
    }
}

/// Merge multiple per-pane event streams into a single deterministic replay stream.
///
/// Each input stream must be sorted by pane-local sequence. The output is sorted
/// by [`ReplayOrder`] — lexicographic `(global_sequence, pane_id, sequence)`.
pub fn merge_replay_streams<T, F>(streams: Vec<Vec<T>>, order_key: F) -> Vec<T>
where
    F: Fn(&T) -> ReplayOrder,
{
    let mut all: Vec<T> = streams.into_iter().flatten().collect();
    all.sort_by_key(|item| order_key(item));
    all
}

// ---------------------------------------------------------------------------
// Correlation context
// ---------------------------------------------------------------------------

/// Correlation metadata linking an event to its causal chain.
///
/// This extends the base `RecorderEventCausality` with additional fields
/// for batch grouping and pattern correlation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrelationContext {
    /// Immediate predecessor event in the same pane.
    pub parent_event_id: Option<String>,
    /// Event that triggered this one (e.g., ingress action → egress response).
    pub trigger_event_id: Option<String>,
    /// Root event of the causal chain for end-to-end tracing.
    pub root_event_id: Option<String>,
    /// Batch ID grouping atomically-related events (e.g., workflow step).
    pub batch_id: Option<String>,
}

impl CorrelationContext {
    /// Create an empty correlation context (no causal links).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            parent_event_id: None,
            trigger_event_id: None,
            root_event_id: None,
            batch_id: None,
        }
    }

    /// Create a context linked to a parent event.
    #[must_use]
    pub fn with_parent(parent_id: String) -> Self {
        Self {
            parent_event_id: Some(parent_id),
            trigger_event_id: None,
            root_event_id: None,
            batch_id: None,
        }
    }

    /// Create a context as a response to a triggering event.
    #[must_use]
    pub fn as_response(trigger_id: String, root_id: Option<String>) -> Self {
        Self {
            parent_event_id: None,
            trigger_event_id: Some(trigger_id),
            root_event_id: root_id,
            batch_id: None,
        }
    }

    /// Set the batch ID for atomic event grouping.
    #[must_use]
    pub fn with_batch(mut self, batch_id: String) -> Self {
        self.batch_id = Some(batch_id);
        self
    }

    /// Returns true if this event has any causal links.
    pub fn has_links(&self) -> bool {
        self.parent_event_id.is_some()
            || self.trigger_event_id.is_some()
            || self.root_event_id.is_some()
    }
}

impl Default for CorrelationContext {
    fn default() -> Self {
        Self::empty()
    }
}

// ---------------------------------------------------------------------------
// Correlation tracker — auto-populates parent chains
// ---------------------------------------------------------------------------

/// Tracks the last event per pane to auto-populate parent links.
#[derive(Debug, Default)]
pub struct CorrelationTracker {
    /// Last event ID emitted per pane.
    last_event_per_pane: Mutex<HashMap<u64, String>>,
    /// Active batch IDs per pane.
    active_batches: Mutex<HashMap<u64, String>>,
}

impl CorrelationTracker {
    /// Create a new empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a [`CorrelationContext`] for a new event on `pane_id`.
    ///
    /// Automatically sets `parent_event_id` to the last event from the same
    /// pane, and `batch_id` from any active batch.
    pub fn build_context(
        &self,
        pane_id: u64,
        event_id: &str,
        trigger_id: Option<&str>,
        root_id: Option<&str>,
    ) -> CorrelationContext {
        let parent = {
            let mut map = self.last_event_per_pane.lock().unwrap();
            let prev = map.get(&pane_id).cloned();
            map.insert(pane_id, event_id.to_string());
            prev
        };

        let batch = self.active_batches.lock().unwrap().get(&pane_id).cloned();

        CorrelationContext {
            parent_event_id: parent,
            trigger_event_id: trigger_id.map(String::from),
            root_event_id: root_id.map(String::from),
            batch_id: batch,
        }
    }

    /// Start a named batch for a pane. Events on this pane will carry the
    /// batch ID until [`end_batch`] is called.
    pub fn start_batch(&self, pane_id: u64, batch_id: String) {
        self.active_batches
            .lock()
            .unwrap()
            .insert(pane_id, batch_id);
    }

    /// End the active batch for a pane.
    pub fn end_batch(&self, pane_id: u64) {
        self.active_batches.lock().unwrap().remove(&pane_id);
    }

    /// Clear tracking state for a pane (e.g., pane closed).
    pub fn clear_pane(&self, pane_id: u64) {
        self.last_event_per_pane.lock().unwrap().remove(&pane_id);
        self.active_batches.lock().unwrap().remove(&pane_id);
    }
}

// ---------------------------------------------------------------------------
// Clock skew detection
// ---------------------------------------------------------------------------

/// Policy for handling clock skew between panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClockSkewPolicy {
    /// Ignore skew — use sequence numbers for ordering (default, recommended).
    IgnoreUseSequence,
    /// Flag skew exceeding the threshold but don't alter ordering.
    FlagOnly,
}

impl Default for ClockSkewPolicy {
    fn default() -> Self {
        Self::IgnoreUseSequence
    }
}

/// Detects clock skew anomalies between events.
///
/// Does NOT affect replay ordering — that always uses sequence numbers.
/// Skew detection is purely diagnostic.
#[derive(Debug)]
pub struct ClockSkewDetector {
    /// Threshold in milliseconds above which skew is flagged.
    threshold_ms: u64,
    /// Last observed timestamp per pane.
    last_ts: Mutex<HashMap<u64, u64>>,
    /// Accumulated anomalies.
    anomalies: Mutex<Vec<ClockSkewAnomaly>>,
}

/// A detected clock skew anomaly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSkewAnomaly {
    /// Pane where the anomaly occurred.
    pub pane_id: u64,
    /// The event's occurred_at_ms timestamp.
    pub event_ts_ms: u64,
    /// The previous event's occurred_at_ms on the same pane.
    pub prev_ts_ms: u64,
    /// Signed delta (negative means backwards jump).
    pub delta_ms: i64,
    /// Global sequence of the anomalous event.
    pub global_sequence: u64,
}

impl ClockSkewDetector {
    /// Create a new detector with the given threshold.
    #[must_use]
    pub fn new(threshold_ms: u64) -> Self {
        Self {
            threshold_ms,
            last_ts: Mutex::new(HashMap::new()),
            anomalies: Mutex::new(Vec::new()),
        }
    }

    /// Observe a timestamp for a pane and flag anomalies.
    ///
    /// Returns `Some(anomaly)` if the timestamp jumps backward by more than
    /// `threshold_ms`, or forward by an unreasonable amount (>60s per event).
    pub fn observe(
        &self,
        pane_id: u64,
        occurred_at_ms: u64,
        global_sequence: u64,
    ) -> Option<ClockSkewAnomaly> {
        let mut map = self.last_ts.lock().unwrap();
        let prev = map.insert(pane_id, occurred_at_ms);

        if let Some(prev_ts) = prev {
            let delta = occurred_at_ms as i64 - prev_ts as i64;

            // Backward jump exceeding threshold
            let backward_skew = delta < 0 && delta.unsigned_abs() > self.threshold_ms;
            // Forward jump exceeding 60 seconds (likely clock reset)
            let forward_skew = delta > 60_000;

            if backward_skew || forward_skew {
                let anomaly = ClockSkewAnomaly {
                    pane_id,
                    event_ts_ms: occurred_at_ms,
                    prev_ts_ms: prev_ts,
                    delta_ms: delta,
                    global_sequence,
                };
                self.anomalies.lock().unwrap().push(anomaly.clone());
                return Some(anomaly);
            }
        }

        None
    }

    /// Return all detected anomalies.
    pub fn anomalies(&self) -> Vec<ClockSkewAnomaly> {
        self.anomalies.lock().unwrap().clone()
    }

    /// Return the number of detected anomalies.
    pub fn anomaly_count(&self) -> usize {
        self.anomalies.lock().unwrap().len()
    }

    /// Clear all tracking state.
    pub fn clear(&self) {
        self.last_ts.lock().unwrap().clear();
        self.anomalies.lock().unwrap().clear();
    }

    /// Clear state for a specific pane.
    pub fn clear_pane(&self, pane_id: u64) {
        self.last_ts.lock().unwrap().remove(&pane_id);
    }
}

// ---------------------------------------------------------------------------
// Replay validation
// ---------------------------------------------------------------------------

/// Validates that a stream of events satisfies the deterministic replay invariants.
///
/// Returns a list of violations found (empty = valid).
pub fn validate_replay_order(orders: &[ReplayOrder]) -> Vec<ReplayOrderViolation> {
    let mut violations = Vec::new();

    for window in orders.windows(2) {
        let prev = &window[0];
        let curr = &window[1];

        // Global sequence must be non-decreasing
        if curr.global_sequence < prev.global_sequence {
            violations.push(ReplayOrderViolation::GlobalSequenceRegression {
                position: 1, // relative position in window
                prev: *prev,
                curr: *curr,
            });
        }

        // Within the same pane, pane sequence must be strictly increasing
        if prev.pane_id == curr.pane_id && curr.sequence <= prev.sequence {
            violations.push(ReplayOrderViolation::PaneSequenceRegression {
                pane_id: prev.pane_id,
                prev: *prev,
                curr: *curr,
            });
        }

        // Global sequence must be strictly increasing (no duplicates)
        if curr.global_sequence == prev.global_sequence
            && curr.pane_id == prev.pane_id
            && curr.sequence == prev.sequence
        {
            violations.push(ReplayOrderViolation::DuplicateOrder { order: *curr });
        }
    }

    violations
}

/// A violation of the deterministic replay ordering invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOrderViolation {
    /// Global sequence went backward.
    GlobalSequenceRegression {
        position: usize,
        prev: ReplayOrder,
        curr: ReplayOrder,
    },
    /// Per-pane sequence didn't increase within the same pane.
    PaneSequenceRegression {
        pane_id: u64,
        prev: ReplayOrder,
        curr: ReplayOrder,
    },
    /// Exact duplicate ordering key.
    DuplicateOrder { order: ReplayOrder },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- SequenceAssigner tests --

    #[test]
    fn assigner_starts_at_zero() {
        let assigner = SequenceAssigner::new();
        assert_eq!(assigner.current_global(), 0);
        assert_eq!(assigner.current_pane(0), 0);
        assert_eq!(assigner.pane_count(), 0);
    }

    #[test]
    fn assign_single_pane_monotonic() {
        let assigner = SequenceAssigner::new();
        for i in 0..10 {
            let (pane_seq, global_seq) = assigner.assign(0);
            assert_eq!(pane_seq, i);
            assert_eq!(global_seq, i);
        }
    }

    #[test]
    fn assign_multiple_panes_interleaved() {
        let assigner = SequenceAssigner::new();

        let (p0_s0, g0) = assigner.assign(0);
        let (p1_s0, g1) = assigner.assign(1);
        let (p0_s1, g2) = assigner.assign(0);
        let (p1_s1, g3) = assigner.assign(1);
        let (p2_s0, g4) = assigner.assign(2);

        // Per-pane sequences start at 0 independently
        assert_eq!(p0_s0, 0);
        assert_eq!(p1_s0, 0);
        assert_eq!(p0_s1, 1);
        assert_eq!(p1_s1, 1);
        assert_eq!(p2_s0, 0);

        // Global sequence is strictly monotonic
        assert_eq!(g0, 0);
        assert_eq!(g1, 1);
        assert_eq!(g2, 2);
        assert_eq!(g3, 3);
        assert_eq!(g4, 4);

        assert_eq!(assigner.pane_count(), 3);
    }

    #[test]
    fn reset_pane_restarts_at_zero() {
        let assigner = SequenceAssigner::new();

        assigner.assign(0);
        assigner.assign(0);
        assert_eq!(assigner.current_pane(0), 2);

        assigner.reset_pane(0);
        assert_eq!(assigner.current_pane(0), 0);

        // After reset, pane starts fresh but global continues
        let (pane_seq, global_seq) = assigner.assign(0);
        assert_eq!(pane_seq, 0);
        assert_eq!(global_seq, 2); // global never resets
    }

    #[test]
    fn assigner_concurrent_safety() {
        use std::sync::Arc;
        use std::thread;

        let assigner = Arc::new(SequenceAssigner::new());
        let mut handles = Vec::new();

        for pane_id in 0..4 {
            let a = Arc::clone(&assigner);
            handles.push(thread::spawn(move || {
                let mut seqs = Vec::new();
                for _ in 0..100 {
                    seqs.push(a.assign(pane_id));
                }
                seqs
            }));
        }

        let mut all_globals = Vec::new();
        for handle in handles {
            let seqs = handle.join().unwrap();
            // Per-pane sequences must be 0..100
            let pane_seqs: Vec<u64> = seqs.iter().map(|(p, _)| *p).collect();
            let expected: Vec<u64> = (0..100).collect();
            assert_eq!(pane_seqs, expected);

            // Collect globals for uniqueness check
            all_globals.extend(seqs.iter().map(|(_, g)| *g));
        }

        // All global sequences must be unique
        all_globals.sort();
        let len_before = all_globals.len();
        all_globals.dedup();
        assert_eq!(
            all_globals.len(),
            len_before,
            "global sequences must be unique"
        );
        assert_eq!(all_globals.len(), 400);
    }

    // -- ReplayOrder tests --

    #[test]
    fn replay_order_sorts_by_global_first() {
        let a = ReplayOrder::new(0, 5, 0);
        let b = ReplayOrder::new(1, 0, 0);
        assert!(a < b);
    }

    #[test]
    fn replay_order_sorts_by_pane_second() {
        let a = ReplayOrder::new(0, 0, 99);
        let b = ReplayOrder::new(0, 1, 0);
        assert!(a < b);
    }

    #[test]
    fn replay_order_sorts_by_sequence_third() {
        let a = ReplayOrder::new(0, 0, 0);
        let b = ReplayOrder::new(0, 0, 1);
        assert!(a < b);
    }

    #[test]
    fn replay_order_is_total() {
        let orders = vec![
            ReplayOrder::new(2, 0, 0),
            ReplayOrder::new(0, 1, 0),
            ReplayOrder::new(1, 0, 1),
            ReplayOrder::new(0, 0, 0),
            ReplayOrder::new(1, 0, 0),
        ];

        let mut sorted = orders.clone();
        sorted.sort();

        assert_eq!(sorted[0], ReplayOrder::new(0, 0, 0));
        assert_eq!(sorted[1], ReplayOrder::new(0, 1, 0));
        assert_eq!(sorted[2], ReplayOrder::new(1, 0, 0));
        assert_eq!(sorted[3], ReplayOrder::new(1, 0, 1));
        assert_eq!(sorted[4], ReplayOrder::new(2, 0, 0));
    }

    #[test]
    fn replay_order_is_before() {
        let a = ReplayOrder::new(0, 0, 0);
        let b = ReplayOrder::new(1, 0, 0);
        assert!(a.is_before(&b));
        assert!(!b.is_before(&a));
        assert!(!a.is_before(&a));
    }

    #[test]
    fn replay_order_is_concurrent() {
        let a = ReplayOrder::new(5, 0, 0);
        let b = ReplayOrder::new(5, 1, 0);
        assert!(a.is_concurrent_with(&b));

        let c = ReplayOrder::new(5, 0, 1);
        assert!(!a.is_concurrent_with(&c)); // same pane, not concurrent
    }

    #[test]
    fn replay_order_serialization_roundtrip() {
        let order = ReplayOrder::new(42, 7, 13);
        let json = serde_json::to_string(&order).unwrap();
        let decoded: ReplayOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(order, decoded);
    }

    // -- merge_replay_streams tests --

    #[test]
    fn merge_empty_streams() {
        let streams: Vec<Vec<ReplayOrder>> = vec![vec![], vec![]];
        let merged = merge_replay_streams(streams, |o| *o);
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_single_stream() {
        let stream = vec![
            ReplayOrder::new(0, 0, 0),
            ReplayOrder::new(1, 0, 1),
            ReplayOrder::new(3, 0, 2),
        ];
        let merged = merge_replay_streams(vec![stream.clone()], |o| *o);
        assert_eq!(merged, stream);
    }

    #[test]
    fn merge_two_interleaved_streams() {
        let pane0 = vec![
            ReplayOrder::new(0, 0, 0),
            ReplayOrder::new(2, 0, 1),
            ReplayOrder::new(4, 0, 2),
        ];
        let pane1 = vec![
            ReplayOrder::new(1, 1, 0),
            ReplayOrder::new(3, 1, 1),
            ReplayOrder::new(5, 1, 2),
        ];

        let merged = merge_replay_streams(vec![pane0, pane1], |o| *o);
        let globals: Vec<u64> = merged.iter().map(|o| o.global_sequence).collect();
        assert_eq!(globals, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn merge_is_deterministic() {
        let pane0 = vec![ReplayOrder::new(1, 0, 0), ReplayOrder::new(3, 0, 1)];
        let pane1 = vec![ReplayOrder::new(0, 1, 0), ReplayOrder::new(2, 1, 1)];

        let merged1 = merge_replay_streams(vec![pane0.clone(), pane1.clone()], |o| *o);
        let merged2 = merge_replay_streams(vec![pane1, pane0], |o| *o);

        // Same result regardless of input order
        assert_eq!(merged1, merged2);
    }

    // -- CorrelationContext tests --

    #[test]
    fn empty_context_has_no_links() {
        let ctx = CorrelationContext::empty();
        assert!(!ctx.has_links());
        assert!(ctx.parent_event_id.is_none());
        assert!(ctx.trigger_event_id.is_none());
        assert!(ctx.root_event_id.is_none());
        assert!(ctx.batch_id.is_none());
    }

    #[test]
    fn context_with_parent() {
        let ctx = CorrelationContext::with_parent("evt-001".into());
        assert!(ctx.has_links());
        assert_eq!(ctx.parent_event_id.as_deref(), Some("evt-001"));
    }

    #[test]
    fn context_as_response() {
        let ctx = CorrelationContext::as_response("trigger-1".into(), Some("root-0".into()));
        assert!(ctx.has_links());
        assert_eq!(ctx.trigger_event_id.as_deref(), Some("trigger-1"));
        assert_eq!(ctx.root_event_id.as_deref(), Some("root-0"));
    }

    #[test]
    fn context_with_batch() {
        let ctx = CorrelationContext::empty().with_batch("batch-42".into());
        assert_eq!(ctx.batch_id.as_deref(), Some("batch-42"));
    }

    #[test]
    fn context_serialization_roundtrip() {
        let ctx = CorrelationContext {
            parent_event_id: Some("p1".into()),
            trigger_event_id: Some("t1".into()),
            root_event_id: Some("r1".into()),
            batch_id: Some("b1".into()),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let decoded: CorrelationContext = serde_json::from_str(&json).unwrap();
        assert_eq!(ctx, decoded);
    }

    // -- CorrelationTracker tests --

    #[test]
    fn tracker_auto_populates_parent() {
        let tracker = CorrelationTracker::new();

        let ctx1 = tracker.build_context(0, "evt-1", None, None);
        assert!(ctx1.parent_event_id.is_none()); // first event has no parent

        let ctx2 = tracker.build_context(0, "evt-2", None, None);
        assert_eq!(ctx2.parent_event_id.as_deref(), Some("evt-1"));

        let ctx3 = tracker.build_context(0, "evt-3", None, None);
        assert_eq!(ctx3.parent_event_id.as_deref(), Some("evt-2"));
    }

    #[test]
    fn tracker_independent_per_pane() {
        let tracker = CorrelationTracker::new();

        tracker.build_context(0, "p0-evt-1", None, None);
        tracker.build_context(1, "p1-evt-1", None, None);

        let ctx_p0 = tracker.build_context(0, "p0-evt-2", None, None);
        let ctx_p1 = tracker.build_context(1, "p1-evt-2", None, None);

        assert_eq!(ctx_p0.parent_event_id.as_deref(), Some("p0-evt-1"));
        assert_eq!(ctx_p1.parent_event_id.as_deref(), Some("p1-evt-1"));
    }

    #[test]
    fn tracker_preserves_trigger_and_root() {
        let tracker = CorrelationTracker::new();

        let ctx = tracker.build_context(0, "evt-1", Some("trig-0"), Some("root-0"));
        assert_eq!(ctx.trigger_event_id.as_deref(), Some("trig-0"));
        assert_eq!(ctx.root_event_id.as_deref(), Some("root-0"));
    }

    #[test]
    fn tracker_batch_propagation() {
        let tracker = CorrelationTracker::new();

        tracker.start_batch(0, "batch-1".into());

        let ctx1 = tracker.build_context(0, "evt-1", None, None);
        assert_eq!(ctx1.batch_id.as_deref(), Some("batch-1"));

        let ctx2 = tracker.build_context(0, "evt-2", None, None);
        assert_eq!(ctx2.batch_id.as_deref(), Some("batch-1"));

        tracker.end_batch(0);

        let ctx3 = tracker.build_context(0, "evt-3", None, None);
        assert!(ctx3.batch_id.is_none());
    }

    #[test]
    fn tracker_clear_pane_resets_state() {
        let tracker = CorrelationTracker::new();
        tracker.start_batch(0, "b1".into());
        tracker.build_context(0, "evt-1", None, None);

        tracker.clear_pane(0);

        let ctx = tracker.build_context(0, "evt-2", None, None);
        assert!(ctx.parent_event_id.is_none()); // parent chain cleared
        assert!(ctx.batch_id.is_none()); // batch cleared
    }

    // -- ClockSkewDetector tests --

    #[test]
    fn no_anomaly_for_monotonic_timestamps() {
        let detector = ClockSkewDetector::new(100);

        assert!(detector.observe(0, 1000, 0).is_none());
        assert!(detector.observe(0, 1200, 1).is_none());
        assert!(detector.observe(0, 1400, 2).is_none());

        assert_eq!(detector.anomaly_count(), 0);
    }

    #[test]
    fn detects_backward_jump() {
        let detector = ClockSkewDetector::new(50);

        detector.observe(0, 1000, 0);
        let anomaly = detector.observe(0, 900, 1); // 100ms backward

        assert!(anomaly.is_some());
        let a = anomaly.unwrap();
        assert_eq!(a.pane_id, 0);
        assert_eq!(a.delta_ms, -100);
        assert_eq!(a.global_sequence, 1);
    }

    #[test]
    fn ignores_small_backward_jump() {
        let detector = ClockSkewDetector::new(100);

        detector.observe(0, 1000, 0);
        let anomaly = detector.observe(0, 950, 1); // only 50ms backward, under threshold

        assert!(anomaly.is_none());
    }

    #[test]
    fn detects_large_forward_jump() {
        let detector = ClockSkewDetector::new(100);

        detector.observe(0, 1000, 0);
        let anomaly = detector.observe(0, 70_000, 1); // 69s forward jump

        assert!(anomaly.is_some());
    }

    #[test]
    fn independent_per_pane_tracking() {
        let detector = ClockSkewDetector::new(100);

        detector.observe(0, 1000, 0);
        detector.observe(1, 5000, 1); // different pane, higher ts is fine

        // Each pane tracks independently
        let a = detector.observe(0, 1100, 2); // normal progression for pane 0
        assert!(a.is_none());
    }

    #[test]
    fn clear_pane_resets_tracking() {
        let detector = ClockSkewDetector::new(100);

        detector.observe(0, 1000, 0);
        detector.clear_pane(0);

        // After clear, next observe is treated as first (no anomaly)
        let a = detector.observe(0, 500, 1);
        assert!(a.is_none());
    }

    #[test]
    fn anomalies_accumulate() {
        let detector = ClockSkewDetector::new(50);

        detector.observe(0, 1000, 0);
        detector.observe(0, 800, 1); // backward
        detector.observe(0, 700, 2); // backward again

        assert_eq!(detector.anomaly_count(), 2);
        let all = detector.anomalies();
        assert_eq!(all.len(), 2);
    }

    // -- validate_replay_order tests --

    #[test]
    fn valid_order_produces_no_violations() {
        let orders = vec![
            ReplayOrder::new(0, 0, 0),
            ReplayOrder::new(1, 1, 0),
            ReplayOrder::new(2, 0, 1),
            ReplayOrder::new(3, 1, 1),
        ];
        assert!(validate_replay_order(&orders).is_empty());
    }

    #[test]
    fn detects_global_regression() {
        let orders = vec![
            ReplayOrder::new(5, 0, 0),
            ReplayOrder::new(3, 0, 1), // regression
        ];
        let violations = validate_replay_order(&orders);
        assert_eq!(violations.len(), 1);
        matches!(
            &violations[0],
            ReplayOrderViolation::GlobalSequenceRegression { .. }
        );
    }

    #[test]
    fn detects_pane_sequence_regression() {
        let orders = vec![
            ReplayOrder::new(0, 0, 5),
            ReplayOrder::new(1, 0, 3), // same pane, sequence went down
        ];
        let violations = validate_replay_order(&orders);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, ReplayOrderViolation::PaneSequenceRegression { .. }))
        );
    }

    #[test]
    fn detects_duplicate_order() {
        let orders = vec![
            ReplayOrder::new(0, 0, 0),
            ReplayOrder::new(0, 0, 0), // exact duplicate
        ];
        let violations = validate_replay_order(&orders);
        assert!(
            violations
                .iter()
                .any(|v| matches!(v, ReplayOrderViolation::DuplicateOrder { .. }))
        );
    }

    #[test]
    fn empty_and_single_are_valid() {
        assert!(validate_replay_order(&[]).is_empty());
        assert!(validate_replay_order(&[ReplayOrder::new(0, 0, 0)]).is_empty());
    }

    // -- Integration: assigner → replay order → validation --

    #[test]
    fn assigner_produces_valid_replay_order() {
        let assigner = SequenceAssigner::new();

        let mut orders = Vec::new();
        for _ in 0..10 {
            let (pane_seq, global_seq) = assigner.assign(0);
            orders.push(ReplayOrder::new(global_seq, 0, pane_seq));
        }
        for _ in 0..10 {
            let (pane_seq, global_seq) = assigner.assign(1);
            orders.push(ReplayOrder::new(global_seq, 1, pane_seq));
        }

        // Sort for replay
        orders.sort();

        // Validate
        let violations = validate_replay_order(&orders);
        assert!(
            violations.is_empty(),
            "Assigner-produced order has violations: {:?}",
            violations
        );
    }

    #[test]
    fn interleaved_assign_produces_valid_order() {
        let assigner = SequenceAssigner::new();

        let mut orders = Vec::new();
        // Interleave 3 panes
        for _ in 0..20 {
            for pane_id in 0..3 {
                let (pane_seq, global_seq) = assigner.assign(pane_id);
                orders.push(ReplayOrder::new(global_seq, pane_id, pane_seq));
            }
        }

        orders.sort();
        let violations = validate_replay_order(&orders);
        assert!(
            violations.is_empty(),
            "Interleaved order has violations: {:?}",
            violations
        );
    }

    #[test]
    fn merge_and_validate_roundtrip() {
        let assigner = SequenceAssigner::new();

        // Build per-pane streams
        let mut pane0 = Vec::new();
        let mut pane1 = Vec::new();

        for _ in 0..5 {
            let (ps, gs) = assigner.assign(0);
            pane0.push(ReplayOrder::new(gs, 0, ps));
            let (ps, gs) = assigner.assign(1);
            pane1.push(ReplayOrder::new(gs, 1, ps));
        }

        // Merge
        let merged = merge_replay_streams(vec![pane0, pane1], |o| *o);

        // Validate
        assert_eq!(merged.len(), 10);
        let violations = validate_replay_order(&merged);
        assert!(violations.is_empty());

        // Verify determinism: merge again in different order
        let assigner2 = SequenceAssigner::new();
        let mut s0 = Vec::new();
        let mut s1 = Vec::new();
        for _ in 0..5 {
            let (ps, gs) = assigner2.assign(0);
            s0.push(ReplayOrder::new(gs, 0, ps));
            let (ps, gs) = assigner2.assign(1);
            s1.push(ReplayOrder::new(gs, 1, ps));
        }

        let merged2 = merge_replay_streams(vec![s1, s0], |o| *o);
        assert_eq!(
            merged, merged2,
            "merge must be deterministic regardless of input order"
        );
    }
}
