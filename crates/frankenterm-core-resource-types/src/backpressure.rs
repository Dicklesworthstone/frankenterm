//! Backpressure policy for the ft watcher pipeline.
//!
//! Monitors capture channel and storage write queue depths, classifies the
//! system into four tiers (Green / Yellow / Red / Black), and provides
//! actionable signals that upstream tasks use to shed load gracefully.
//!
//! See `docs/backpressure-policy.md` for the full design specification.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ─── Telemetry ──────────────────────────────────────────────────────

/// Operational telemetry counters for the backpressure manager.
///
/// All counters are `AtomicU64` because `BackpressureManager` methods take `&self`.
#[derive(Debug, Default)]
pub struct BackpressureTelemetry {
    /// Total evaluate() calls.
    evaluations: AtomicU64,
    /// Total classify() calls.
    classifications: AtomicU64,
    /// Tier transitions (evaluate() calls that changed tier).
    transitions: AtomicU64,
    /// pause_pane() calls.
    panes_paused: AtomicU64,
    /// resume_pane() calls.
    panes_resumed: AtomicU64,
    /// resume_all_panes() calls.
    resume_alls: AtomicU64,
}

impl BackpressureTelemetry {
    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> BackpressureTelemetrySnapshot {
        BackpressureTelemetrySnapshot {
            evaluations: self.evaluations.load(Ordering::Relaxed),
            classifications: self.classifications.load(Ordering::Relaxed),
            transitions: self.transitions.load(Ordering::Relaxed),
            panes_paused: self.panes_paused.load(Ordering::Relaxed),
            panes_resumed: self.panes_resumed.load(Ordering::Relaxed),
            resume_alls: self.resume_alls.load(Ordering::Relaxed),
        }
    }
}

/// Serializable snapshot of backpressure telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackpressureTelemetrySnapshot {
    pub evaluations: u64,
    pub classifications: u64,
    pub transitions: u64,
    pub panes_paused: u64,
    pub panes_resumed: u64,
    pub resume_alls: u64,
}

// ─── Tier ────────────────────────────────────────────────────────────

/// Backpressure severity tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureTier {
    /// All queues below warning thresholds.
    Green,
    /// Capture ≥ yellow threshold OR write ≥ yellow threshold.
    Yellow,
    /// Capture ≥ red threshold OR write ≥ red threshold.
    Red,
    /// Queue near saturation.
    Black,
}

impl std::fmt::Display for BackpressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "GREEN"),
            Self::Yellow => write!(f, "YELLOW"),
            Self::Red => write!(f, "RED"),
            Self::Black => write!(f, "BLACK"),
        }
    }
}

impl BackpressureTier {
    /// Numeric value for gauge metrics (0–3).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Yellow => 1,
            Self::Red => 2,
            Self::Black => 3,
        }
    }
}

// ─── Configuration ───────────────────────────────────────────────────

/// Backpressure policy configuration.
///
/// All thresholds are expressed as fractions (0.0–1.0) of the respective
/// queue capacity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackpressureConfig {
    /// Enable the backpressure policy.
    pub enabled: bool,

    /// How often to sample queue depths (milliseconds).
    pub check_interval_ms: u64,

    // ── Capture channel thresholds ──
    /// Fraction of capture channel capacity that triggers Yellow.
    pub yellow_capture: f64,
    /// Fraction of capture channel capacity that triggers Red.
    pub red_capture: f64,

    // ── Write queue thresholds ──
    /// Fraction of write queue capacity that triggers Yellow.
    pub yellow_write: f64,
    /// Fraction of write queue capacity that triggers Red.
    pub red_write: f64,

    /// Minimum time (ms) in an elevated tier before allowing downgrade.
    pub hysteresis_ms: u64,

    // ── Yellow tier actions ──
    /// Idle pane poll interval multiplier.
    pub idle_poll_backoff_factor: f64,
    /// Fraction of lowest-priority panes to skip for pattern detection.
    pub skip_detection_ratio: f64,

    // ── Red tier actions ──
    /// Fraction of lowest-priority panes to pause.
    pub pause_ratio: f64,
    /// Maximum segments buffered in the persistence task before dropping.
    pub max_buffered_segments: usize,

    // ── Recovery ──
    /// Resume one paused pane every N milliseconds during recovery.
    pub recovery_resume_interval_ms: u64,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_ms: 500,
            yellow_capture: 0.50,
            red_capture: 0.75,
            yellow_write: 0.60,
            red_write: 0.80,
            hysteresis_ms: 2000,
            idle_poll_backoff_factor: 2.0,
            skip_detection_ratio: 0.25,
            pause_ratio: 0.50,
            max_buffered_segments: 100,
            recovery_resume_interval_ms: 500,
        }
    }
}

impl BackpressureConfig {
    /// Validate that every threshold is finite, non-negative, and ordered
    /// yellow ≤ red. Returns a human-readable error on the first violation.
    ///
    /// [ft-atcr0] TOML supports `nan`/`inf` float literals, and serde passes
    /// them through to this struct unchanged. If any f64 threshold reaches
    /// `classify` as NaN, every `>=` comparison against it is false and the
    /// tier silently resolves to Green — disabling every backpressure
    /// action at the exact moment the operator tried to configure one.
    /// Same failure shape as ft-761tz (disk_pressure), same disposition:
    /// reject at the config boundary AND fail closed in classify().
    pub fn validate(&self) -> std::result::Result<(), String> {
        for (name, value) in [
            ("yellow_capture", self.yellow_capture),
            ("red_capture", self.red_capture),
            ("yellow_write", self.yellow_write),
            ("red_write", self.red_write),
            ("idle_poll_backoff_factor", self.idle_poll_backoff_factor),
            ("skip_detection_ratio", self.skip_detection_ratio),
            ("pause_ratio", self.pause_ratio),
        ] {
            if !value.is_finite() {
                return Err(format!(
                    "backpressure config {name} must be finite, got {value}"
                ));
            }
            if value < 0.0 {
                return Err(format!(
                    "backpressure config {name} must be non-negative, got {value}"
                ));
            }
        }
        if self.yellow_capture > self.red_capture {
            return Err(format!(
                "backpressure config yellow_capture ({}) must be ≤ red_capture ({})",
                self.yellow_capture, self.red_capture
            ));
        }
        if self.yellow_write > self.red_write {
            return Err(format!(
                "backpressure config yellow_write ({}) must be ≤ red_write ({})",
                self.yellow_write, self.red_write
            ));
        }
        Ok(())
    }
}

// ─── Queue Observation ───────────────────────────────────────────────

/// A point-in-time reading of queue depths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueDepths {
    pub capture_depth: usize,
    pub capture_capacity: usize,
    pub write_depth: usize,
    pub write_capacity: usize,
}

impl QueueDepths {
    /// Capture queue fill ratio (0.0–1.0).
    #[must_use]
    pub fn capture_ratio(&self) -> f64 {
        if self.capture_capacity == 0 {
            return 0.0;
        }
        self.capture_depth as f64 / self.capture_capacity as f64
    }

    /// Write queue fill ratio (0.0–1.0).
    #[must_use]
    pub fn write_ratio(&self) -> f64 {
        if self.write_capacity == 0 {
            return 0.0;
        }
        self.write_depth as f64 / self.write_capacity as f64
    }
}

// ─── Snapshot ────────────────────────────────────────────────────────

/// Serialisable snapshot of the current backpressure state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureSnapshot {
    pub tier: BackpressureTier,
    pub timestamp_epoch_ms: u64,
    pub capture_depth: usize,
    pub capture_capacity: usize,
    pub write_depth: usize,
    pub write_capacity: usize,
    pub duration_in_tier_ms: u64,
    pub transitions: u64,
    pub paused_panes: Vec<u64>,
}

// ─── Load-Proof Timeline Artifact ────────────────────────────────────

/// Schema version for machine-readable backpressure load-proof timelines.
pub const BACKPRESSURE_TIMELINE_SCHEMA_VERSION: u32 = 1;

/// Upper bound applied to per-run timeline events.
pub const BACKPRESSURE_TIMELINE_MAX_EVENTS: usize = 4096;

/// Upper bound applied to each event's evidence list.
pub const BACKPRESSURE_TIMELINE_MAX_EVIDENCE: usize = 8;

/// Machine-readable load-test artifact for backpressure and memory pressure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackpressureTimelineArtifact {
    pub schema_version: u32,
    pub run_id: String,
    pub scenario: String,
    pub generated_at_ms: u64,
    pub host_label: String,
    pub events: Vec<BackpressureTimelineEvent>,
    pub summary: BackpressureTimelineSummary,
    #[serde(default)]
    pub truncated_events: usize,
}

impl BackpressureTimelineArtifact {
    /// Build an untruncated artifact and derive the summary from events.
    #[must_use]
    pub fn new(
        run_id: impl Into<String>,
        scenario: impl Into<String>,
        generated_at_ms: u64,
        host_label: impl Into<String>,
        events: Vec<BackpressureTimelineEvent>,
    ) -> Self {
        Self::from_events_bounded(
            run_id,
            scenario,
            generated_at_ms,
            host_label,
            events,
            usize::MAX,
        )
    }

    /// Build a bounded artifact suitable for repeated scale-lab proof runs.
    #[must_use]
    pub fn from_events_bounded(
        run_id: impl Into<String>,
        scenario: impl Into<String>,
        generated_at_ms: u64,
        host_label: impl Into<String>,
        events: Vec<BackpressureTimelineEvent>,
        max_events: usize,
    ) -> Self {
        let total_events = events.len();
        let bounded_events: Vec<BackpressureTimelineEvent> = events
            .into_iter()
            .take(max_events)
            .map(BackpressureTimelineEvent::bounded)
            .collect();
        let truncated_events = total_events.saturating_sub(bounded_events.len());
        let summary = BackpressureTimelineSummary::from_events(&bounded_events);

        Self {
            schema_version: BACKPRESSURE_TIMELINE_SCHEMA_VERSION,
            run_id: run_id.into(),
            scenario: scenario.into(),
            generated_at_ms,
            host_label: host_label.into(),
            events: bounded_events,
            summary,
            truncated_events,
        }
    }
}

/// One timestamped resource-pressure event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackpressureTimelineEvent {
    pub timestamp_ms: u64,
    pub kind: BackpressureTimelineEventKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<BackpressureTimelineEvidence>,
}

impl BackpressureTimelineEvent {
    /// Build an event without auxiliary evidence.
    #[must_use]
    pub fn new(timestamp_ms: u64, kind: BackpressureTimelineEventKind) -> Self {
        Self {
            timestamp_ms,
            kind,
            evidence: Vec::new(),
        }
    }

    /// Build an event with bounded evidence links.
    #[must_use]
    pub fn with_evidence(
        timestamp_ms: u64,
        kind: BackpressureTimelineEventKind,
        evidence: Vec<BackpressureTimelineEvidence>,
    ) -> Self {
        Self {
            timestamp_ms,
            kind,
            evidence,
        }
        .bounded()
    }

    #[must_use]
    fn bounded(mut self) -> Self {
        self.evidence.truncate(BACKPRESSURE_TIMELINE_MAX_EVIDENCE);
        self
    }
}

/// Event kinds emitted into the load-proof timeline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureTimelineEventKind {
    QueueSample {
        tier: BackpressureTier,
        capture_depth: usize,
        capture_capacity: usize,
        write_depth: usize,
        write_capacity: usize,
    },
    TierTransition {
        from: BackpressureTier,
        to: BackpressureTier,
        reason: String,
    },
    Throttle {
        action: BackpressureThrottleAction,
        pane_id: Option<u64>,
        reason: String,
    },
    SegmentDrop {
        pane_id: u64,
        dropped_segments: u64,
        reason: String,
    },
    MemoryTierTransition {
        pane_id: u64,
        from: BackpressureMemoryTier,
        to: BackpressureMemoryTier,
        bytes: u64,
        reason: String,
    },
    ApiLatency {
        surface: BackpressureApiSurface,
        operation: String,
        latency_ms: u64,
        ok: bool,
    },
}

/// Normalized throttle action vocabulary for proof artifacts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureThrottleAction {
    PollBackoff,
    SkipDetection,
    PausePane,
    DeferFts,
    DropSegment,
    ReduceBufferLimit,
}

/// Normalized memory residency tiers for pressure transition explanations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureMemoryTier {
    Hot,
    Warm,
    Cold,
    Evicted,
}

/// API surface labels for latency samples in proof artifacts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureApiSurface {
    Cli,
    Robot,
    Mcp,
    Web,
    WatchDaemon,
}

/// Bounded evidence pointer attached to a timeline event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackpressureTimelineEvidence {
    pub source: BackpressureTimelineEvidenceSource,
    pub key: String,
    pub value: String,
}

impl BackpressureTimelineEvidence {
    /// Build a key/value evidence pointer.
    #[must_use]
    pub fn new(
        source: BackpressureTimelineEvidenceSource,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        Self {
            source,
            key: key.into(),
            value: value.into(),
        }
    }
}

/// Source kind for timeline evidence entries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackpressureTimelineEvidenceSource {
    QueueDepth,
    BackpressureSnapshot,
    MemoryTier,
    ApiTrace,
    ScaleLabFixture,
}

/// Summary counters derived from the bounded event list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackpressureTimelineSummary {
    pub total_events: usize,
    pub queue_samples: usize,
    pub tier_transitions: usize,
    pub throttle_events: usize,
    pub segment_drop_events: usize,
    pub memory_tier_transitions: usize,
    pub api_latency_events: usize,
    pub highest_backpressure_tier: Option<BackpressureTier>,
    pub max_capture_depth: usize,
    pub max_write_depth: usize,
    pub total_segments_dropped: u64,
    pub distinct_throttled_panes: usize,
    pub memory_bytes_moved: u64,
    pub max_api_latency_ms: u64,
    pub failed_api_calls: usize,
}

impl BackpressureTimelineSummary {
    /// Derive artifact-level counters from events.
    #[must_use]
    pub fn from_events(events: &[BackpressureTimelineEvent]) -> Self {
        let mut summary = Self {
            total_events: events.len(),
            ..Self::default()
        };
        let mut throttled_panes = HashSet::new();

        for event in events {
            match &event.kind {
                BackpressureTimelineEventKind::QueueSample {
                    tier,
                    capture_depth,
                    write_depth,
                    ..
                } => {
                    summary.queue_samples += 1;
                    summary.observe_tier(*tier);
                    summary.max_capture_depth = summary.max_capture_depth.max(*capture_depth);
                    summary.max_write_depth = summary.max_write_depth.max(*write_depth);
                }
                BackpressureTimelineEventKind::TierTransition { to, .. } => {
                    summary.tier_transitions += 1;
                    summary.observe_tier(*to);
                }
                BackpressureTimelineEventKind::Throttle { pane_id, .. } => {
                    summary.throttle_events += 1;
                    if let Some(pane_id) = pane_id {
                        throttled_panes.insert(*pane_id);
                    }
                }
                BackpressureTimelineEventKind::SegmentDrop {
                    dropped_segments, ..
                } => {
                    summary.segment_drop_events += 1;
                    summary.total_segments_dropped = summary
                        .total_segments_dropped
                        .saturating_add(*dropped_segments);
                }
                BackpressureTimelineEventKind::MemoryTierTransition { bytes, .. } => {
                    summary.memory_tier_transitions += 1;
                    summary.memory_bytes_moved = summary.memory_bytes_moved.saturating_add(*bytes);
                }
                BackpressureTimelineEventKind::ApiLatency { latency_ms, ok, .. } => {
                    summary.api_latency_events += 1;
                    summary.max_api_latency_ms = summary.max_api_latency_ms.max(*latency_ms);
                    if !ok {
                        summary.failed_api_calls += 1;
                    }
                }
            }
        }

        summary.distinct_throttled_panes = throttled_panes.len();
        summary
    }

    fn observe_tier(&mut self, tier: BackpressureTier) {
        self.highest_backpressure_tier = Some(
            self.highest_backpressure_tier
                .map_or(tier, |current| current.max(tier)),
        );
    }
}

// ─── Metrics ─────────────────────────────────────────────────────────

/// Counters tracked across the lifetime of the backpressure manager.
///
/// [ft-0e179] `segments_dropped` is the aggregate counter; the per-pane
/// attribution lives in `segments_dropped_by_pane`. Both are updated
/// atomically from a single call to [`record_segment_dropped`], so the
/// aggregate and the per-pane sum always agree. A scalar view is cheap
/// (dashboards, "total drops in last minute"); the per-pane view is what
/// the 3am on-call needs to answer "were the drops from the pane I'm
/// debugging?".
#[derive(Debug, Default)]
pub struct BackpressureMetrics {
    pub yellow_entries: AtomicU64,
    pub red_entries: AtomicU64,
    pub black_entries: AtomicU64,
    /// Aggregate segments-dropped counter. Preserved for the existing
    /// `wa_backpressure_segments_dropped_total` metric wire contract.
    /// [ft-0e179] Update path is `record_segment_dropped(pane_id)`, not
    /// raw `fetch_add` — that keeps the per-pane map in sync.
    pub segments_dropped: AtomicU64,
    pub gaps_emitted: AtomicU64,
    pub detection_skipped: AtomicU64,
    pub fts_deferred: AtomicU64,
    /// [ft-0e179] Per-pane drop counter. Keyed by `pane_id`, value is the
    /// cumulative count of segments dropped from that pane's capture
    /// stream. Populated on-demand — a pane that has never been dropped
    /// from contributes no entry.
    ///
    /// Access pattern: read-lock fast path for panes that already have an
    /// entry (common case after the first drop), write-lock only on the
    /// very first drop from a given pane. This keeps the hot drop path
    /// contention-free once the map is warm.
    dropped_by_pane: RwLock<HashMap<u64, AtomicU64>>,
}

impl BackpressureMetrics {
    /// [ft-0e179] Record a segment drop with per-pane attribution.
    ///
    /// Increments both the aggregate `segments_dropped` counter and the
    /// per-pane entry for `pane_id`. Callers should route every drop-site
    /// through this method — a raw `segments_dropped.fetch_add(1, ...)`
    /// would leave the per-pane view stale and silently reintroduce the
    /// 3am-debugging gap ft-0e179 fixes.
    pub fn record_segment_dropped(&self, pane_id: u64) {
        self.segments_dropped.fetch_add(1, Ordering::Relaxed);

        // Fast path: pane already in the map → one atomic increment, no
        // write-lock contention with concurrent drops from other panes.
        {
            let guard = self
                .dropped_by_pane
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(counter) = guard.get(&pane_id) {
                counter.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }

        // Slow path: first drop from this pane. Upgrade to write-lock and
        // `entry().or_insert_with()` handles the race with a concurrent
        // writer that beats us here.
        let mut guard = self
            .dropped_by_pane
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .entry(pane_id)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// [ft-0e179] Total drops across all panes (cheap, lock-free).
    #[must_use]
    pub fn segments_dropped_total(&self) -> u64 {
        self.segments_dropped.load(Ordering::Relaxed)
    }

    /// [ft-0e179] Drops attributed to a single pane. `0` if the pane has
    /// never had a segment dropped (either because it's healthy or because
    /// it was never seen — the two cases are indistinguishable by design
    /// to keep the map allocation-free on first-seen panes).
    #[must_use]
    pub fn segments_dropped_for_pane(&self, pane_id: u64) -> u64 {
        let guard = self
            .dropped_by_pane
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .get(&pane_id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// [ft-0e179] Snapshot of `(pane_id, drop_count)` for every pane that
    /// has had at least one segment dropped. Returned as a `HashMap` so
    /// the caller can render either a total, a per-pane histogram, or a
    /// top-N offenders list without a second pass.
    ///
    /// Concurrency: acquires a write-lock briefly. The write-lock
    /// (rather than read-lock) is intentional — it serialises against
    /// concurrent first-drops so the returned snapshot is internally
    /// consistent even if a drop is mid-flight for a pane that isn't yet
    /// in the map.
    #[must_use]
    pub fn segments_dropped_by_pane(&self) -> HashMap<u64, u64> {
        let guard = self
            .dropped_by_pane
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .map(|(pane_id, counter)| (*pane_id, counter.load(Ordering::Relaxed)))
            .collect()
    }

    /// [ft-0e179] Number of distinct panes that have had at least one
    /// drop — useful for dashboard gauges ("dropping from 3 panes").
    #[must_use]
    pub fn panes_with_drops(&self) -> usize {
        let guard = self
            .dropped_by_pane
            .read()
            .unwrap_or_else(|e| e.into_inner());
        guard.len()
    }

    /// Remove the drop-attribution entry for `pane_id`, returning `true`
    /// if an entry was actually removed.
    ///
    /// Must be called from `PaneDestroyed` teardown paths (mirrors the
    /// ft-l6v1r fix that added cleanup for `pane_activity_tracker` in
    /// `runtime.rs`). Without this, the `dropped_by_pane` HashMap grows
    /// monotonically on long-running aggregators — every pane that has
    /// ever seen a drop leaves an `AtomicU64` behind for the life of the
    /// runtime, and worse, if a pane_id is reused by the mux after
    /// teardown the new pane inherits the dead pane's drop count.
    ///
    /// Found during a review pass over the last-20-commits swarm diff:
    /// bd8a715e landed the per-pane attribution map without wiring a
    /// teardown hook; handle_native_event's `NativeEvent::PaneDestroyed`
    /// arm and the `diff.closed_panes` loop in `ObservationRuntime::run`
    /// now call this helper alongside `remove_runtime_pane_state_for_pane`.
    pub fn cleanup_pane(&self, pane_id: u64) -> bool {
        let mut guard = self
            .dropped_by_pane
            .write()
            .unwrap_or_else(|e| e.into_inner());
        guard.remove(&pane_id).is_some()
    }
}

// ─── Manager ─────────────────────────────────────────────────────────

/// Evaluates queue depths and manages tier transitions with hysteresis.
pub struct BackpressureManager {
    config: BackpressureConfig,
    /// Combined (tier, tier_entered_at) pair under a single lock.
    ///
    /// [ft-89fl5] Previously two separate RwLocks (`current_tier` +
    /// `tier_entered_at`). A concurrent evaluator could observe a
    /// half-applied transition: new tier + stale entered_at. The stale
    /// entered_at made the hysteresis check at line 540 use a timestamp
    /// from a *previous* tier, treating the elapsed window as already
    /// satisfied and allowing a downgrade that should have been
    /// blocked. Collapsing into one lock makes the transition atomic
    /// from every observer's perspective: either the old tier plus old
    /// entered_at, or the new tier plus new entered_at, never a cross pair.
    tier_state: RwLock<TierState>,
    transition_count: AtomicU64,
    paused_panes: Arc<RwLock<HashSet<u64>>>,
    pub metrics: BackpressureMetrics,
    telemetry: BackpressureTelemetry,
}

/// Atomic snapshot of the manager's current tier + entry timestamp.
/// Used internally to keep the (tier, entered_at) pair consistent under
/// concurrent evaluate() calls (ft-89fl5).
#[derive(Debug, Clone, Copy)]
struct TierState {
    tier: BackpressureTier,
    entered_at: Instant,
}

impl BackpressureManager {
    /// Create a new manager with the given configuration.
    #[must_use]
    pub fn new(config: BackpressureConfig) -> Self {
        Self {
            config,
            tier_state: RwLock::new(TierState {
                tier: BackpressureTier::Green,
                entered_at: Instant::now(),
            }),
            transition_count: AtomicU64::new(0),
            paused_panes: Arc::new(RwLock::new(HashSet::new())),
            metrics: BackpressureMetrics::default(),
            telemetry: BackpressureTelemetry::default(),
        }
    }

    /// Whether the policy is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Snapshot the current telemetry counters.
    pub fn telemetry(&self) -> &BackpressureTelemetry {
        &self.telemetry
    }

    /// Current tier (lock-free read when only an approximate value is needed).
    #[must_use]
    pub fn current_tier(&self) -> BackpressureTier {
        self.tier_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .tier
    }

    /// Classify queue depths into a tier without applying any state change.
    #[must_use]
    pub fn classify(&self, depths: &QueueDepths) -> BackpressureTier {
        self.telemetry
            .classifications
            .fetch_add(1, Ordering::Relaxed);
        let cr = depths.capture_ratio();
        let wr = depths.write_ratio();

        // [ft-atcr0] Fail closed on non-finite inputs or thresholds.
        // `cr >= NaN` is always false, so a single NaN threshold would
        // silently classify every queue depth as Green — the exact
        // inverse of what backpressure exists to do. Same disposition as
        // ft-761tz (disk_pressure classify_tier returning Black on NaN
        // usage_fraction): when we cannot prove the queue is safe, the
        // only sound answer is Black. The config-load path also has a
        // validate() gate that rejects NaN at boundary crossing; this
        // is the defense-in-depth layer for call sites that bypass
        // validation (tests, ad-hoc construction).
        if !cr.is_finite()
            || !wr.is_finite()
            || !self.config.yellow_capture.is_finite()
            || !self.config.red_capture.is_finite()
            || !self.config.yellow_write.is_finite()
            || !self.config.red_write.is_finite()
        {
            return BackpressureTier::Black;
        }

        // Black: near saturation. The absolute "within N slots of full"
        // guard is only meaningful once the queue is already highly filled;
        // otherwise tiny capacities (for example 0/1 or 0/5) would trip
        // `saturating_sub` and classify empty or lightly-loaded queues as
        // Black. Keep the large-queue early warning, but require a high
        // fill ratio before the absolute margin can escalate to Black.
        let capture_saturated = depths.capture_capacity > 0
            && (cr >= 0.995
                || (cr >= 0.95
                    && depths.capture_depth >= depths.capture_capacity.saturating_sub(5)));
        let write_saturated = depths.write_capacity > 0
            && (wr >= 0.995
                || (wr >= 0.95 && depths.write_depth >= depths.write_capacity.saturating_sub(100)));

        if capture_saturated || write_saturated {
            BackpressureTier::Black
        } else if cr >= self.config.red_capture || wr >= self.config.red_write {
            BackpressureTier::Red
        } else if cr >= self.config.yellow_capture || wr >= self.config.yellow_write {
            BackpressureTier::Yellow
        } else {
            BackpressureTier::Green
        }
    }

    /// Evaluate queue depths and apply a tier transition if warranted.
    ///
    /// Returns `Some((old, new))` when the tier changes, `None` otherwise.
    pub fn evaluate(&self, depths: &QueueDepths) -> Option<(BackpressureTier, BackpressureTier)> {
        self.telemetry.evaluations.fetch_add(1, Ordering::Relaxed);
        if !self.config.enabled {
            return None;
        }

        let proposed = self.classify(depths);

        // [ft-89fl5] Single lock acquisition covers the read-check-
        // write sequence so another concurrent evaluator cannot
        // observe a half-applied transition (new tier + stale
        // entered_at). The write guard is held for the duration of
        // the check AND the apply, so `if proposed == current` ...
        // decision and the final mutation are consistent.
        let mut guard = self.tier_state.write().unwrap_or_else(|e| e.into_inner());
        let current = guard.tier;

        if proposed == current {
            return None;
        }

        // Upgrades (toward Black) are immediate.
        // Downgrades require hysteresis.
        if proposed < current {
            let elapsed_ms =
                u64::try_from(guard.entered_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            if elapsed_ms < self.config.hysteresis_ms {
                return None; // too soon to downgrade
            }
        }

        // Apply transition atomically under the held write lock.
        guard.tier = proposed;
        guard.entered_at = Instant::now();
        // Release the write lock before the fetch_add + tracing::warn
        // so those don't hold the lock any longer than necessary.
        drop(guard);
        self.transition_count.fetch_add(1, Ordering::Relaxed);
        self.telemetry.transitions.fetch_add(1, Ordering::Relaxed);

        match proposed {
            BackpressureTier::Yellow => {
                self.metrics.yellow_entries.fetch_add(1, Ordering::Relaxed);
            }
            BackpressureTier::Red => {
                self.metrics.red_entries.fetch_add(1, Ordering::Relaxed);
            }
            BackpressureTier::Black => {
                self.metrics.black_entries.fetch_add(1, Ordering::Relaxed);
            }
            BackpressureTier::Green => {}
        }

        tracing::warn!(
            old_tier = %current,
            new_tier = %proposed,
            capture_ratio = format_args!("{:.1}%", depths.capture_ratio() * 100.0),
            write_ratio = format_args!("{:.1}%", depths.write_ratio() * 100.0),
            "backpressure tier transition"
        );

        Some((current, proposed))
    }

    // ── Pane pause management ────────────────────────────────────────

    /// Mark a pane as paused due to backpressure.
    pub fn pause_pane(&self, pane_id: u64) {
        self.paused_panes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pane_id);
        self.telemetry.panes_paused.fetch_add(1, Ordering::Relaxed);
    }

    /// Resume a previously paused pane.
    pub fn resume_pane(&self, pane_id: u64) {
        self.paused_panes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pane_id);
        self.telemetry.panes_resumed.fetch_add(1, Ordering::Relaxed);
    }

    /// Resume all paused panes (e.g. on recovery to Green).
    pub fn resume_all_panes(&self) {
        self.paused_panes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.telemetry.resume_alls.fetch_add(1, Ordering::Relaxed);
    }

    /// Check if a pane is currently paused.
    #[must_use]
    pub fn is_pane_paused(&self, pane_id: u64) -> bool {
        self.paused_panes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&pane_id)
    }

    /// List currently paused pane IDs.
    #[must_use]
    pub fn paused_pane_ids(&self) -> Vec<u64> {
        let guard = self.paused_panes.read().unwrap_or_else(|e| e.into_inner());
        let mut ids: Vec<u64> = guard.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    // ── Configuration access ─────────────────────────────────────────

    /// Idle poll backoff factor for Yellow tier.
    #[must_use]
    pub fn idle_poll_backoff_factor(&self) -> f64 {
        self.config.idle_poll_backoff_factor
    }

    /// Fraction of low-priority panes to skip for detection in Yellow.
    #[must_use]
    pub fn skip_detection_ratio(&self) -> f64 {
        self.config.skip_detection_ratio
    }

    /// Fraction of low-priority panes to pause in Red.
    #[must_use]
    pub fn pause_ratio(&self) -> f64 {
        self.config.pause_ratio
    }

    /// Maximum buffered segments in the persistence task.
    #[must_use]
    pub fn max_buffered_segments(&self) -> usize {
        self.config.max_buffered_segments
    }

    /// Interval between resuming individual panes during recovery.
    #[must_use]
    pub fn recovery_resume_interval_ms(&self) -> u64 {
        self.config.recovery_resume_interval_ms
    }

    // ── Snapshot ─────────────────────────────────────────────────────

    /// Produce a serialisable snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self, depths: &QueueDepths) -> BackpressureSnapshot {
        // [ft-89fl5] Single read covers both tier and entered_at so
        // the duration_in_tier_ms can't straddle a concurrent
        // transition.
        let state = *self.tier_state.read().unwrap_or_else(|e| e.into_inner());
        let tier = state.tier;
        let entered = state.entered_at;
        let now_epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        BackpressureSnapshot {
            tier,
            timestamp_epoch_ms: now_epoch_ms,
            capture_depth: depths.capture_depth,
            capture_capacity: depths.capture_capacity,
            write_depth: depths.write_depth,
            write_capacity: depths.write_capacity,
            duration_in_tier_ms: u64::try_from(entered.elapsed().as_millis()).unwrap_or(u64::MAX),
            transitions: self.transition_count.load(Ordering::Relaxed),
            paused_panes: self.paused_pane_ids(),
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_manager() -> BackpressureManager {
        BackpressureManager::new(BackpressureConfig::default())
    }

    fn depths(capture: usize, cap_cap: usize, write: usize, wr_cap: usize) -> QueueDepths {
        QueueDepths {
            capture_depth: capture,
            capture_capacity: cap_cap,
            write_depth: write,
            write_capacity: wr_cap,
        }
    }

    #[test]
    fn initial_state_is_green() {
        let m = default_manager();
        assert_eq!(m.current_tier(), BackpressureTier::Green);
    }

    // ── [ft-atcr0] NaN-threshold fail-closed + config validation ───────

    /// Defense-in-depth: if a BackpressureManager is constructed with a
    /// NaN threshold (bypassing validate(), e.g. via direct struct
    /// literal in tests or ad-hoc callers), `classify()` must return
    /// Black rather than silently falling through to Green.
    #[test]
    fn classify_returns_black_on_nan_config_threshold() {
        let config = BackpressureConfig {
            yellow_capture: f64::NAN,
            ..BackpressureConfig::default()
        };
        let m = BackpressureManager::new(config);

        let depths = QueueDepths {
            capture_depth: 50,
            capture_capacity: 100,
            write_depth: 50,
            write_capacity: 100,
        };
        assert_eq!(
            m.classify(&depths),
            BackpressureTier::Black,
            "NaN yellow_capture must fail closed to Black, not silently Green"
        );
    }

    #[test]
    fn classify_returns_black_on_inf_threshold() {
        let config = BackpressureConfig {
            red_capture: f64::INFINITY,
            ..BackpressureConfig::default()
        };
        let m = BackpressureManager::new(config);

        let depths = QueueDepths {
            capture_depth: 50,
            capture_capacity: 100,
            write_depth: 50,
            write_capacity: 100,
        };
        assert_eq!(
            m.classify(&depths),
            BackpressureTier::Black,
            "infinite red_capture must fail closed to Black"
        );
    }

    #[test]
    fn config_validate_accepts_defaults() {
        assert!(BackpressureConfig::default().validate().is_ok());
    }

    #[test]
    fn config_validate_rejects_nan_threshold() {
        let config = BackpressureConfig {
            yellow_capture: f64::NAN,
            ..BackpressureConfig::default()
        };
        let err = config.validate().expect_err("NaN must be rejected");
        assert!(err.contains("yellow_capture"));
        assert!(err.contains("finite"));
    }

    #[test]
    fn config_validate_rejects_inf_threshold() {
        let config = BackpressureConfig {
            red_write: f64::INFINITY,
            ..BackpressureConfig::default()
        };
        let err = config.validate().expect_err("infinity must be rejected");
        assert!(err.contains("red_write"));
    }

    #[test]
    fn config_validate_rejects_negative_threshold() {
        let config = BackpressureConfig {
            skip_detection_ratio: -0.1,
            ..BackpressureConfig::default()
        };
        let err = config.validate().expect_err("negative must be rejected");
        assert!(err.contains("skip_detection_ratio"));
        assert!(err.contains("non-negative"));
    }

    #[test]
    fn config_validate_rejects_yellow_above_red() {
        let config = BackpressureConfig {
            yellow_capture: 0.9,
            red_capture: 0.5, // yellow > red is a misconfiguration
            ..BackpressureConfig::default()
        };
        let err = config
            .validate()
            .expect_err("yellow > red must be rejected");
        assert!(err.contains("yellow_capture"));
        assert!(err.contains("red_capture"));
    }

    #[test]
    fn config_validate_rejects_yellow_write_above_red_write() {
        let config = BackpressureConfig {
            yellow_write: 0.95,
            red_write: 0.7,
            ..BackpressureConfig::default()
        };
        let err = config.validate().expect_err("yellow_write > red_write");
        assert!(err.contains("yellow_write"));
        assert!(err.contains("red_write"));
    }

    // ── [ft-0e179] per-pane drop attribution ───────────────────────────

    #[test]
    fn record_segment_dropped_bumps_aggregate_and_per_pane() {
        let m = BackpressureMetrics::default();
        assert_eq!(m.segments_dropped_total(), 0);
        assert_eq!(m.segments_dropped_for_pane(42), 0);

        m.record_segment_dropped(42);

        assert_eq!(m.segments_dropped_total(), 1);
        assert_eq!(m.segments_dropped_for_pane(42), 1);
        // Other panes still read 0 — no entry created for them.
        assert_eq!(m.segments_dropped_for_pane(7), 0);
    }

    #[test]
    fn record_segment_dropped_attributes_distinct_panes_independently() {
        let m = BackpressureMetrics::default();

        // 3 drops from pane 4167, 1 drop from 4203, 2 from 4167 again.
        m.record_segment_dropped(4167);
        m.record_segment_dropped(4167);
        m.record_segment_dropped(4167);
        m.record_segment_dropped(4203);
        m.record_segment_dropped(4167);
        m.record_segment_dropped(4167);

        assert_eq!(m.segments_dropped_total(), 6);
        assert_eq!(m.segments_dropped_for_pane(4167), 5);
        assert_eq!(m.segments_dropped_for_pane(4203), 1);
        // Untouched pane still absent from the map.
        assert_eq!(m.segments_dropped_for_pane(9999), 0);
    }

    #[test]
    fn segments_dropped_by_pane_returns_full_snapshot() {
        let m = BackpressureMetrics::default();
        m.record_segment_dropped(1);
        m.record_segment_dropped(1);
        m.record_segment_dropped(2);
        m.record_segment_dropped(3);
        m.record_segment_dropped(3);
        m.record_segment_dropped(3);

        let snap = m.segments_dropped_by_pane();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap.get(&1).copied(), Some(2));
        assert_eq!(snap.get(&2).copied(), Some(1));
        assert_eq!(snap.get(&3).copied(), Some(3));
        // Aggregate matches the sum.
        let sum: u64 = snap.values().sum();
        assert_eq!(sum, m.segments_dropped_total());
    }

    #[test]
    fn panes_with_drops_counts_distinct_keys() {
        let m = BackpressureMetrics::default();
        assert_eq!(m.panes_with_drops(), 0);

        m.record_segment_dropped(10);
        assert_eq!(m.panes_with_drops(), 1);

        m.record_segment_dropped(10); // same pane again → still 1
        assert_eq!(m.panes_with_drops(), 1);

        m.record_segment_dropped(20);
        assert_eq!(m.panes_with_drops(), 2);
    }

    // [review] Pin the teardown invariant that the PaneDestroyed path in
    // runtime.rs relies on: cleanup_pane(X) removes only X's attribution
    // entry, leaves every other pane's counter untouched, is idempotent
    // on panes that were never inserted (the 99% "healthy pane"
    // equivalence class), and preserves the aggregate
    // `segments_dropped_total()` so post-mortem accounting across a
    // pane's lifetime still sums correctly after the pane is gone.
    #[test]
    fn cleanup_pane_removes_only_target_pane() {
        let m = BackpressureMetrics::default();
        m.record_segment_dropped(1);
        m.record_segment_dropped(1);
        m.record_segment_dropped(2);
        m.record_segment_dropped(3);
        assert_eq!(m.panes_with_drops(), 3);
        assert_eq!(m.segments_dropped_total(), 4);

        assert!(m.cleanup_pane(2));
        assert_eq!(m.panes_with_drops(), 2);
        assert_eq!(m.segments_dropped_for_pane(1), 2);
        assert_eq!(m.segments_dropped_for_pane(2), 0);
        assert_eq!(m.segments_dropped_for_pane(3), 1);
        // Aggregate is preserved — it's a monotonic lifetime counter, not
        // a by-pane sum, so cleanup does NOT rewrite the total.
        assert_eq!(m.segments_dropped_total(), 4);

        // Idempotent on a pane that was never inserted (healthy-pane case).
        assert!(!m.cleanup_pane(999));
        // Idempotent on a pane that was just removed.
        assert!(!m.cleanup_pane(2));
    }

    #[test]
    fn record_segment_dropped_concurrent_same_pane() {
        use std::sync::Arc;
        use std::thread;

        // Hot-path stress: N threads, each recording K drops for the
        // same pane. Final count must equal N×K (atomic fetch_add on
        // the per-pane entry — no lost updates under contention).
        let m = Arc::new(BackpressureMetrics::default());
        let n_threads = 8;
        let per_thread = 1000;

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let m = Arc::clone(&m);
                thread::spawn(move || {
                    for _ in 0..per_thread {
                        m.record_segment_dropped(12345);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let expected = (n_threads * per_thread) as u64;
        assert_eq!(m.segments_dropped_total(), expected);
        assert_eq!(m.segments_dropped_for_pane(12345), expected);
    }

    #[test]
    fn record_segment_dropped_concurrent_many_panes_first_seen_race() {
        use std::sync::Arc;
        use std::thread;

        // Stress the first-seen upgrade path: many threads, each
        // targeting a distinct pane. The write-lock race in
        // `record_segment_dropped` must land every drop; no pane's
        // first drop can be silently lost if two threads happen to
        // both hit the slow path at the same time for different keys.
        let m = Arc::new(BackpressureMetrics::default());
        let n_panes: u64 = 64;

        let handles: Vec<_> = (0..n_panes)
            .map(|pane_id| {
                let m = Arc::clone(&m);
                thread::spawn(move || {
                    m.record_segment_dropped(pane_id);
                    m.record_segment_dropped(pane_id);
                    m.record_segment_dropped(pane_id);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(m.segments_dropped_total(), n_panes * 3);
        assert_eq!(m.panes_with_drops(), n_panes as usize);
        for pane in 0..n_panes {
            assert_eq!(m.segments_dropped_for_pane(pane), 3);
        }
    }

    #[test]
    fn classify_green() {
        let m = default_manager();
        let d = depths(100, 1024, 500, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Green);
    }

    #[test]
    fn classify_yellow_capture() {
        let m = default_manager();
        // 512/1024 = 50% → Yellow
        let d = depths(512, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Yellow);
    }

    #[test]
    fn classify_yellow_write() {
        let m = default_manager();
        // 6000/10000 = 60% → Yellow
        let d = depths(0, 1024, 6000, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Yellow);
    }

    #[test]
    fn classify_red_capture() {
        let m = default_manager();
        // 768/1024 = 75% → Red
        let d = depths(768, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Red);
    }

    #[test]
    fn classify_red_write() {
        let m = default_manager();
        // 8000/10000 = 80% → Red
        let d = depths(0, 1024, 8000, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Red);
    }

    #[test]
    fn classify_black_capture_saturated() {
        let m = default_manager();
        // 1022/1024 (within 5 of capacity) → Black
        let d = depths(1022, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
    }

    #[test]
    fn classify_black_write_saturated() {
        let m = default_manager();
        // 9950/10000 (within 100 of capacity) → Black
        let d = depths(0, 1024, 9950, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
    }

    #[test]
    fn classify_zero_capacity_is_green() {
        let m = default_manager();
        let d = depths(0, 0, 0, 0);
        assert_eq!(m.classify(&d), BackpressureTier::Green);
    }

    #[test]
    fn evaluate_upgrades_immediately() {
        let m = default_manager();
        let d = depths(768, 1024, 0, 10_000); // Red
        let result = m.evaluate(&d);
        assert_eq!(
            result,
            Some((BackpressureTier::Green, BackpressureTier::Red))
        );
        assert_eq!(m.current_tier(), BackpressureTier::Red);
    }

    #[test]
    fn evaluate_downgrade_blocked_by_hysteresis() {
        let config = BackpressureConfig {
            hysteresis_ms: 60_000, // 60 seconds
            ..BackpressureConfig::default()
        };
        let m = BackpressureManager::new(config);

        // First: upgrade to Red
        let d_red = depths(768, 1024, 0, 10_000);
        m.evaluate(&d_red);
        assert_eq!(m.current_tier(), BackpressureTier::Red);

        // Attempt downgrade to Green: should be blocked by hysteresis
        let d_green = depths(10, 1024, 100, 10_000);
        let result = m.evaluate(&d_green);
        assert!(result.is_none());
        assert_eq!(m.current_tier(), BackpressureTier::Red);
    }

    #[test]
    fn evaluate_no_change_returns_none() {
        let m = default_manager();
        let d = depths(10, 1024, 100, 10_000); // Green
        let result = m.evaluate(&d);
        assert!(result.is_none());
    }

    #[test]
    fn evaluate_disabled_returns_none() {
        let config = BackpressureConfig {
            enabled: false,
            ..BackpressureConfig::default()
        };
        let m = BackpressureManager::new(config);

        let d = depths(1024, 1024, 0, 10_000); // Would be Black
        let result = m.evaluate(&d);
        assert!(result.is_none());
        assert_eq!(m.current_tier(), BackpressureTier::Green);
    }

    #[test]
    fn pane_pause_lifecycle() {
        let m = default_manager();

        assert!(!m.is_pane_paused(1));
        assert!(m.paused_pane_ids().is_empty());

        m.pause_pane(1);
        m.pause_pane(3);
        assert!(m.is_pane_paused(1));
        assert!(m.is_pane_paused(3));
        assert!(!m.is_pane_paused(2));
        assert_eq!(m.paused_pane_ids(), vec![1, 3]);

        m.resume_pane(1);
        assert!(!m.is_pane_paused(1));
        assert!(m.is_pane_paused(3));

        m.resume_all_panes();
        assert!(m.paused_pane_ids().is_empty());
    }

    #[test]
    fn snapshot_reflects_state() {
        let m = default_manager();
        let d = depths(768, 1024, 2000, 10_000);
        m.evaluate(&d); // → Red

        m.pause_pane(5);
        m.pause_pane(9);

        let snap = m.snapshot(&d);
        assert_eq!(snap.tier, BackpressureTier::Red);
        assert_eq!(snap.capture_depth, 768);
        assert_eq!(snap.capture_capacity, 1024);
        assert_eq!(snap.paused_panes, vec![5, 9]);
        assert!(snap.transitions >= 1);
    }

    #[test]
    fn tier_ordering() {
        assert!(BackpressureTier::Green < BackpressureTier::Yellow);
        assert!(BackpressureTier::Yellow < BackpressureTier::Red);
        assert!(BackpressureTier::Red < BackpressureTier::Black);
    }

    #[test]
    fn tier_display() {
        assert_eq!(BackpressureTier::Green.to_string(), "GREEN");
        assert_eq!(BackpressureTier::Yellow.to_string(), "YELLOW");
        assert_eq!(BackpressureTier::Red.to_string(), "RED");
        assert_eq!(BackpressureTier::Black.to_string(), "BLACK");
    }

    #[test]
    fn tier_as_u8() {
        assert_eq!(BackpressureTier::Green.as_u8(), 0);
        assert_eq!(BackpressureTier::Yellow.as_u8(), 1);
        assert_eq!(BackpressureTier::Red.as_u8(), 2);
        assert_eq!(BackpressureTier::Black.as_u8(), 3);
    }

    #[test]
    fn queue_depths_ratios() {
        let d = depths(512, 1024, 5000, 10_000);
        assert!((d.capture_ratio() - 0.5).abs() < f64::EPSILON);
        assert!((d.write_ratio() - 0.5).abs() < f64::EPSILON);

        let zero = depths(0, 0, 0, 0);
        assert!((zero.capture_ratio()).abs() < f64::EPSILON);
        assert!((zero.write_ratio()).abs() < f64::EPSILON);
    }

    #[test]
    fn config_default_thresholds() {
        let c = BackpressureConfig::default();
        assert!(c.enabled);
        assert!((c.yellow_capture - 0.50).abs() < f64::EPSILON);
        assert!((c.red_capture - 0.75).abs() < f64::EPSILON);
        assert!((c.yellow_write - 0.60).abs() < f64::EPSILON);
        assert!((c.red_write - 0.80).abs() < f64::EPSILON);
        assert_eq!(c.hysteresis_ms, 2000);
    }

    #[test]
    fn metrics_increment() {
        let m = default_manager();

        // Green → Yellow
        let d = depths(512, 1024, 0, 10_000);
        m.evaluate(&d);
        assert_eq!(m.metrics.yellow_entries.load(Ordering::Relaxed), 1);

        // Yellow → Red (upgrade is immediate)
        let d = depths(768, 1024, 0, 10_000);
        m.evaluate(&d);
        assert_eq!(m.metrics.red_entries.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let snap = BackpressureSnapshot {
            tier: BackpressureTier::Yellow,
            timestamp_epoch_ms: 1_700_000_000_000,
            capture_depth: 500,
            capture_capacity: 1024,
            write_depth: 100,
            write_capacity: 10_000,
            duration_in_tier_ms: 5000,
            transitions: 3,
            paused_panes: vec![1, 5],
        };

        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BackpressureSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tier, BackpressureTier::Yellow);
        assert_eq!(parsed.capture_depth, 500);
        assert_eq!(parsed.paused_panes, vec![1, 5]);
    }

    // -----------------------------------------------------------------------
    // Tier serde
    // -----------------------------------------------------------------------

    #[test]
    fn tier_serde_roundtrip_all_variants() {
        for tier in [
            BackpressureTier::Green,
            BackpressureTier::Yellow,
            BackpressureTier::Red,
            BackpressureTier::Black,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let back: BackpressureTier = serde_json::from_str(&json).unwrap();
            assert_eq!(back, tier);
        }
    }

    #[test]
    fn tier_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&BackpressureTier::Green).unwrap(),
            "\"green\""
        );
        assert_eq!(
            serde_json::to_string(&BackpressureTier::Black).unwrap(),
            "\"black\""
        );
    }

    #[test]
    fn tier_as_u8_matches_ord() {
        let tiers = [
            BackpressureTier::Green,
            BackpressureTier::Yellow,
            BackpressureTier::Red,
            BackpressureTier::Black,
        ];
        for w in tiers.windows(2) {
            assert!(w[0].as_u8() < w[1].as_u8());
            assert!(w[0] < w[1]);
        }
    }

    // -----------------------------------------------------------------------
    // Config serde
    // -----------------------------------------------------------------------

    #[test]
    fn config_serde_roundtrip() {
        let config = BackpressureConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: BackpressureConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.enabled, config.enabled);
        assert!((back.yellow_capture - config.yellow_capture).abs() < f64::EPSILON);
        assert!((back.red_capture - config.red_capture).abs() < f64::EPSILON);
        assert_eq!(back.hysteresis_ms, config.hysteresis_ms);
        assert_eq!(back.max_buffered_segments, config.max_buffered_segments);
    }

    #[test]
    fn config_partial_json_uses_defaults() {
        let json = r#"{"enabled": false}"#;
        let config: BackpressureConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        // All other fields should be defaults.
        let def = BackpressureConfig::default();
        assert!((config.yellow_capture - def.yellow_capture).abs() < f64::EPSILON);
        assert_eq!(config.check_interval_ms, def.check_interval_ms);
    }

    // -----------------------------------------------------------------------
    // Classify boundary conditions
    // -----------------------------------------------------------------------

    #[test]
    fn classify_just_below_yellow_capture_is_green() {
        let m = default_manager();
        // 49.9% capture → Green (threshold is 50%)
        let d = depths(511, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Green);
    }

    #[test]
    fn classify_just_below_red_capture_is_yellow() {
        let m = default_manager();
        // 74.9% capture → Yellow (red threshold is 75%)
        let d = depths(767, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Yellow);
    }

    #[test]
    fn classify_both_queues_elevated_uses_worst() {
        let m = default_manager();
        // Capture at Yellow level, write at Red level → Red wins.
        let d = depths(512, 1024, 8000, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Red);
    }

    #[test]
    fn classify_black_exactly_at_saturation_boundary() {
        let m = default_manager();
        // Capture: capacity - 5 exactly → Black.
        let d = depths(1019, 1024, 0, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
        // One less → not saturated.
        let d2 = depths(1018, 1024, 0, 10_000);
        assert_ne!(m.classify(&d2), BackpressureTier::Black);
    }

    #[test]
    fn classify_write_saturation_boundary_at_100() {
        let m = default_manager();
        // Write: capacity - 100 exactly → Black.
        let d = depths(0, 1024, 9900, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
        // One less → not saturated via this check.
        let d2 = depths(0, 1024, 9899, 10_000);
        assert_ne!(m.classify(&d2), BackpressureTier::Black);
    }

    #[test]
    fn classify_full_capacity_is_black() {
        let m = default_manager();
        let d = depths(1024, 1024, 10_000, 10_000);
        assert_eq!(m.classify(&d), BackpressureTier::Black);
    }

    #[test]
    fn classify_small_capture_capacity_does_not_blackhole_empty_queue() {
        let m = default_manager();

        // Before ft-5 tick audit fix, `saturating_sub(5)` turned every
        // capacity <= 5 capture queue into an always-Black queue because
        // even depth 0 satisfies `depth >= 0`. Empty and lightly-loaded
        // tiny queues must stay out of emergency mode.
        assert_eq!(
            m.classify(&depths(0, 5, 0, 10_000)),
            BackpressureTier::Green
        );
        assert_eq!(
            m.classify(&depths(1, 5, 0, 10_000)),
            BackpressureTier::Green
        );
        assert_eq!(
            m.classify(&depths(5, 5, 0, 10_000)),
            BackpressureTier::Black
        );
    }

    #[test]
    fn classify_small_write_capacity_requires_high_fill_for_black() {
        let m = default_manager();

        // Same regression shape on the write queue: `capacity.saturating_sub(100)`
        // collapses to zero for capacity <= 100. Small queues should reach Black
        // only when they are actually near full, not merely because they exist.
        assert_eq!(
            m.classify(&depths(0, 1024, 0, 100)),
            BackpressureTier::Green
        );
        assert_eq!(m.classify(&depths(0, 1024, 80, 100)), BackpressureTier::Red);
        assert_eq!(
            m.classify(&depths(0, 1024, 95, 100)),
            BackpressureTier::Black
        );
    }

    // -----------------------------------------------------------------------
    // QueueDepths edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn queue_depths_one_capacity_full() {
        let d = depths(1, 1, 1, 1);
        assert!((d.capture_ratio() - 1.0).abs() < f64::EPSILON);
        assert!((d.write_ratio() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn queue_depths_empty_queues() {
        let d = depths(0, 1024, 0, 10_000);
        assert!(d.capture_ratio().abs() < f64::EPSILON);
        assert!(d.write_ratio().abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Evaluate: multiple upgrades
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_multiple_upgrades_without_hysteresis_block() {
        let m = default_manager();

        // Green → Yellow
        let d_y = depths(512, 1024, 0, 10_000);
        let r = m.evaluate(&d_y);
        assert_eq!(r, Some((BackpressureTier::Green, BackpressureTier::Yellow)));

        // Yellow → Red (upgrade is immediate)
        let d_r = depths(768, 1024, 0, 10_000);
        let r = m.evaluate(&d_r);
        assert_eq!(r, Some((BackpressureTier::Yellow, BackpressureTier::Red)));

        // Red → Black (upgrade is immediate)
        let d_b = depths(1022, 1024, 0, 10_000);
        let r = m.evaluate(&d_b);
        assert_eq!(r, Some((BackpressureTier::Red, BackpressureTier::Black)));
        assert_eq!(m.current_tier(), BackpressureTier::Black);
    }

    // -----------------------------------------------------------------------
    // Metrics accumulation
    // -----------------------------------------------------------------------

    #[test]
    fn metrics_black_entry_counted() {
        let m = default_manager();
        let d = depths(1022, 1024, 0, 10_000); // Black
        m.evaluate(&d);
        assert_eq!(m.metrics.black_entries.load(Ordering::Relaxed), 1);
    }

    // -----------------------------------------------------------------------
    // Pane management edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn resume_nonexistent_pane_is_no_op() {
        let m = default_manager();
        m.resume_pane(42); // should not panic
        assert!(m.paused_pane_ids().is_empty());
    }

    #[test]
    fn pause_same_pane_twice_is_idempotent() {
        let m = default_manager();
        m.pause_pane(7);
        m.pause_pane(7);
        assert_eq!(m.paused_pane_ids(), vec![7]);
    }

    // -----------------------------------------------------------------------
    // Config accessor methods
    // -----------------------------------------------------------------------

    #[test]
    fn config_accessor_methods_reflect_config() {
        let config = BackpressureConfig {
            idle_poll_backoff_factor: 4.0,
            skip_detection_ratio: 0.33,
            pause_ratio: 0.75,
            max_buffered_segments: 42,
            recovery_resume_interval_ms: 1234,
            ..BackpressureConfig::default()
        };
        let m = BackpressureManager::new(config);

        assert!((m.idle_poll_backoff_factor() - 4.0).abs() < f64::EPSILON);
        assert!((m.skip_detection_ratio() - 0.33).abs() < f64::EPSILON);
        assert!((m.pause_ratio() - 0.75).abs() < f64::EPSILON);
        assert_eq!(m.max_buffered_segments(), 42);
        assert_eq!(m.recovery_resume_interval_ms(), 1234);
    }

    // -----------------------------------------------------------------------
    // Snapshot edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_green_with_no_paused_panes() {
        let m = default_manager();
        let d = depths(10, 1024, 100, 10_000);
        let snap = m.snapshot(&d);
        assert_eq!(snap.tier, BackpressureTier::Green);
        assert!(snap.paused_panes.is_empty());
        assert_eq!(snap.transitions, 0);
        assert!(snap.timestamp_epoch_ms > 0);
    }

    #[test]
    fn snapshot_serde_all_tiers() {
        for tier in [
            BackpressureTier::Green,
            BackpressureTier::Yellow,
            BackpressureTier::Red,
            BackpressureTier::Black,
        ] {
            let snap = BackpressureSnapshot {
                tier,
                timestamp_epoch_ms: 1_000,
                capture_depth: 0,
                capture_capacity: 100,
                write_depth: 0,
                write_capacity: 100,
                duration_in_tier_ms: 0,
                transitions: 0,
                paused_panes: vec![],
            };
            let json = serde_json::to_string(&snap).unwrap();
            let back: BackpressureSnapshot = serde_json::from_str(&json).unwrap();
            assert_eq!(back.tier, tier);
        }
    }

    // ── Telemetry counter tests ──────────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let monitor = BackpressureManager::new(BackpressureConfig::default());
        let snap = monitor.telemetry().snapshot();
        assert_eq!(snap.evaluations, 0);
        assert_eq!(snap.classifications, 0);
        assert_eq!(snap.transitions, 0);
        assert_eq!(snap.panes_paused, 0);
        assert_eq!(snap.panes_resumed, 0);
        assert_eq!(snap.resume_alls, 0);
    }

    #[test]
    fn telemetry_classify_counted() {
        let monitor = BackpressureManager::new(BackpressureConfig::default());
        let depths = QueueDepths {
            capture_depth: 0,
            capture_capacity: 100,
            write_depth: 0,
            write_capacity: 100,
        };
        let _ = monitor.classify(&depths);
        let _ = monitor.classify(&depths);
        let snap = monitor.telemetry().snapshot();
        assert_eq!(snap.classifications, 2);
    }

    #[test]
    fn telemetry_pause_resume_counted() {
        let monitor = BackpressureManager::new(BackpressureConfig::default());
        monitor.pause_pane(1);
        monitor.pause_pane(2);
        monitor.resume_pane(1);
        monitor.resume_all_panes();
        let snap = monitor.telemetry().snapshot();
        assert_eq!(snap.panes_paused, 2);
        assert_eq!(snap.panes_resumed, 1);
        assert_eq!(snap.resume_alls, 1);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let monitor = BackpressureManager::new(BackpressureConfig::default());
        let depths = QueueDepths {
            capture_depth: 0,
            capture_capacity: 100,
            write_depth: 0,
            write_capacity: 100,
        };
        let _ = monitor.classify(&depths);
        monitor.pause_pane(1);
        let snap = monitor.telemetry().snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: BackpressureTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ── Load-proof timeline artifact tests ─────────────────────────────

    fn sample_queue_event(
        ts: u64,
        depths: QueueDepths,
        tier: BackpressureTier,
    ) -> BackpressureTimelineEvent {
        BackpressureTimelineEvent::new(
            ts,
            BackpressureTimelineEventKind::QueueSample {
                tier,
                capture_depth: depths.capture_depth,
                capture_capacity: depths.capture_capacity,
                write_depth: depths.write_depth,
                write_capacity: depths.write_capacity,
            },
        )
    }

    #[test]
    fn timeline_artifact_bounds_events_and_derives_summary() {
        let events = vec![
            sample_queue_event(1000, depths(10, 100, 20, 100), BackpressureTier::Green),
            sample_queue_event(1100, depths(75, 100, 20, 100), BackpressureTier::Red),
            BackpressureTimelineEvent::new(
                1110,
                BackpressureTimelineEventKind::TierTransition {
                    from: BackpressureTier::Green,
                    to: BackpressureTier::Red,
                    reason: "capture queue crossed red threshold".into(),
                },
            ),
            BackpressureTimelineEvent::new(
                1120,
                BackpressureTimelineEventKind::Throttle {
                    action: BackpressureThrottleAction::PausePane,
                    pane_id: Some(42),
                    reason: "red tier pause ratio selected lowest priority pane".into(),
                },
            ),
            BackpressureTimelineEvent::new(
                1130,
                BackpressureTimelineEventKind::SegmentDrop {
                    pane_id: 42,
                    dropped_segments: 3,
                    reason: "persistence buffer at max_buffered_segments".into(),
                },
            ),
        ];

        let artifact = BackpressureTimelineArtifact::from_events_bounded(
            "run-ft-rhamq",
            "10-pane-replay-pressure",
            1_780_000_000_000,
            "local-replay",
            events,
            4,
        );

        assert_eq!(
            artifact.schema_version,
            BACKPRESSURE_TIMELINE_SCHEMA_VERSION
        );
        assert_eq!(artifact.events.len(), 4);
        assert_eq!(artifact.truncated_events, 1);
        assert_eq!(artifact.summary.total_events, 4);
        assert_eq!(artifact.summary.queue_samples, 2);
        assert_eq!(artifact.summary.tier_transitions, 1);
        assert_eq!(artifact.summary.throttle_events, 1);
        assert_eq!(artifact.summary.segment_drop_events, 0);
        assert_eq!(
            artifact.summary.highest_backpressure_tier,
            Some(BackpressureTier::Red)
        );
        assert_eq!(artifact.summary.max_capture_depth, 75);
        assert_eq!(artifact.summary.distinct_throttled_panes, 1);
    }

    #[test]
    fn timeline_serializes_explainable_throttle_and_memory_transition() {
        let evidence = (0..(BACKPRESSURE_TIMELINE_MAX_EVIDENCE + 2))
            .map(|idx| {
                BackpressureTimelineEvidence::new(
                    BackpressureTimelineEvidenceSource::QueueDepth,
                    format!("queue_sample_{idx}"),
                    format!("capture={idx}"),
                )
            })
            .collect();
        let events = vec![
            BackpressureTimelineEvent::with_evidence(
                2000,
                BackpressureTimelineEventKind::Throttle {
                    action: BackpressureThrottleAction::SkipDetection,
                    pane_id: Some(7),
                    reason: "yellow tier skipped low-priority detection work".into(),
                },
                evidence,
            ),
            BackpressureTimelineEvent::with_evidence(
                2100,
                BackpressureTimelineEventKind::MemoryTierTransition {
                    pane_id: 7,
                    from: BackpressureMemoryTier::Hot,
                    to: BackpressureMemoryTier::Warm,
                    bytes: 8192,
                    reason: "warm scrollback admission reduced hot memory pressure".into(),
                },
                vec![BackpressureTimelineEvidence::new(
                    BackpressureTimelineEvidenceSource::MemoryTier,
                    "scrollback_chunk",
                    "pane=7 bytes=8192",
                )],
            ),
            BackpressureTimelineEvent::new(
                2200,
                BackpressureTimelineEventKind::ApiLatency {
                    surface: BackpressureApiSurface::Robot,
                    operation: "fleet.status".into(),
                    latency_ms: 37,
                    ok: true,
                },
            ),
        ];

        let artifact = BackpressureTimelineArtifact::new(
            "run-ft-rhamq-json",
            "timeline-serialization",
            1_780_000_000_100,
            "fixture-host",
            events,
        );
        let json = serde_json::to_string_pretty(&artifact).unwrap();
        let parsed: BackpressureTimelineArtifact = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, artifact);
        assert_eq!(
            parsed.events[0].evidence.len(),
            BACKPRESSURE_TIMELINE_MAX_EVIDENCE
        );
        assert_eq!(parsed.summary.throttle_events, 1);
        assert_eq!(parsed.summary.memory_tier_transitions, 1);
        assert_eq!(parsed.summary.memory_bytes_moved, 8192);
        assert_eq!(parsed.summary.api_latency_events, 1);
        assert_eq!(parsed.summary.max_api_latency_ms, 37);
        assert!(json.contains("\"skip_detection\""));
        assert!(json.contains("\"warm\""));
        assert!(json.contains("\"fleet.status\""));
    }

    #[test]
    fn timeline_load_fixture_explains_pressure_transition() {
        let manager = default_manager();
        let green = depths(5, 100, 8, 100);
        let red = depths(76, 100, 40, 100);
        let transition = manager.evaluate(&red).expect("fixture must enter red");

        assert_eq!(transition, (BackpressureTier::Green, BackpressureTier::Red));

        let artifact = BackpressureTimelineArtifact::new(
            "run-ft-rhamq-fixture",
            "small-load-fixture",
            1_780_000_000_200,
            "rch-replay",
            vec![
                sample_queue_event(3000, green, manager.classify(&green)),
                sample_queue_event(3010, red, manager.current_tier()),
                BackpressureTimelineEvent::with_evidence(
                    3011,
                    BackpressureTimelineEventKind::TierTransition {
                        from: transition.0,
                        to: transition.1,
                        reason: "capture depth 76/100 crossed red threshold 0.75".into(),
                    },
                    vec![BackpressureTimelineEvidence::new(
                        BackpressureTimelineEvidenceSource::BackpressureSnapshot,
                        "capture_queue",
                        "76/100",
                    )],
                ),
                BackpressureTimelineEvent::new(
                    3020,
                    BackpressureTimelineEventKind::Throttle {
                        action: BackpressureThrottleAction::ReduceBufferLimit,
                        pane_id: None,
                        reason: "red tier applies max_buffered_segments pressure guard".into(),
                    },
                ),
                BackpressureTimelineEvent::new(
                    3030,
                    BackpressureTimelineEventKind::ApiLatency {
                        surface: BackpressureApiSurface::Web,
                        operation: "/stream/events".into(),
                        latency_ms: 125,
                        ok: false,
                    },
                ),
            ],
        );

        assert_eq!(artifact.summary.total_events, 5);
        assert_eq!(artifact.summary.queue_samples, 2);
        assert_eq!(artifact.summary.tier_transitions, 1);
        assert_eq!(artifact.summary.throttle_events, 1);
        assert_eq!(artifact.summary.failed_api_calls, 1);
        assert_eq!(artifact.summary.max_api_latency_ms, 125);
        assert_eq!(
            artifact.summary.highest_backpressure_tier,
            Some(BackpressureTier::Red)
        );

        let transition_event = &artifact.events[2];
        assert_eq!(transition_event.timestamp_ms, 3011);
        assert!(!transition_event.evidence.is_empty());
    }
}
