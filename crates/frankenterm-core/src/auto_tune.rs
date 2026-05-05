//! Auto-tuning configuration parameters based on observed system load.
//!
//! Replaces static configuration with adaptive parameters that respond to
//! actual runtime conditions. Uses proportional control with hysteresis
//! to prevent oscillation.
//!
//! # Control Loop
//!
//! ```text
//! SystemMetrics ──► AutoTuner::tick() ──► TunableParams (clamped + gradual)
//!                       │
//!                       ├── memory pressure → reduce scrollback, increase snapshot interval
//!                       ├── latency pressure → increase poll interval
//!                       └── CPU pressure → reduce pool size, increase poll interval
//! ```
//!
//! See bead `wa-ssm4` for the full design.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

// =============================================================================
// Parameter ranges
// =============================================================================

/// Hard minimum and maximum for each tunable parameter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ParamRange {
    pub min: f64,
    pub max: f64,
}

impl ParamRange {
    /// Clamp a value to this range.
    #[must_use]
    pub fn clamp(&self, v: f64) -> f64 {
        v.clamp(self.min, self.max)
    }

    /// Return true when a value is finite and inside this range.
    #[must_use]
    pub fn contains(&self, v: f64) -> bool {
        v.is_finite() && v >= self.min && v <= self.max
    }
}

/// Default parameter ranges.
pub const POLL_INTERVAL_RANGE: ParamRange = ParamRange {
    min: 100.0,
    max: 10_000.0,
};
pub const SCROLLBACK_LINES_RANGE: ParamRange = ParamRange {
    min: 500.0,
    max: 10_000.0,
};
pub const SNAPSHOT_INTERVAL_RANGE: ParamRange = ParamRange {
    min: 60.0,
    max: 1800.0,
};
pub const POOL_SIZE_RANGE: ParamRange = ParamRange {
    min: 1.0,
    max: 16.0,
};
pub const BACKPRESSURE_THRESHOLD_RANGE: ParamRange = ParamRange { min: 0.3, max: 0.9 };

// =============================================================================
// Tunable parameters
// =============================================================================

/// The set of parameters that the auto-tuner adjusts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TunableParams {
    /// Polling interval for pane state (ms).
    pub poll_interval_ms: f64,
    /// Scrollback lines per pane.
    pub scrollback_lines: f64,
    /// Snapshot interval (seconds).
    pub snapshot_interval_secs: f64,
    /// Connection pool size.
    pub pool_size: f64,
    /// Backpressure threshold (0.0–1.0).
    pub backpressure_threshold: f64,
}

impl Default for TunableParams {
    fn default() -> Self {
        Self {
            poll_interval_ms: 200.0,
            scrollback_lines: 5000.0,
            snapshot_interval_secs: 300.0,
            pool_size: 4.0,
            backpressure_threshold: 0.75,
        }
    }
}

impl TunableParams {
    /// Clamp all parameters to their valid ranges.
    pub fn clamp_to_ranges(&mut self) {
        self.poll_interval_ms = POLL_INTERVAL_RANGE.clamp(self.poll_interval_ms);
        self.scrollback_lines = SCROLLBACK_LINES_RANGE.clamp(self.scrollback_lines);
        self.snapshot_interval_secs = SNAPSHOT_INTERVAL_RANGE.clamp(self.snapshot_interval_secs);
        self.pool_size = POOL_SIZE_RANGE.clamp(self.pool_size);
        self.backpressure_threshold =
            BACKPRESSURE_THRESHOLD_RANGE.clamp(self.backpressure_threshold);
    }

    /// Get the poll interval as an integer (ms).
    #[must_use]
    pub fn poll_interval_ms_u64(&self) -> u64 {
        self.poll_interval_ms.round() as u64
    }

    /// Get the scrollback lines as an integer.
    #[must_use]
    pub fn scrollback_lines_usize(&self) -> usize {
        self.scrollback_lines.round() as usize
    }

    /// Get the snapshot interval as an integer (seconds).
    #[must_use]
    pub fn snapshot_interval_secs_u64(&self) -> u64 {
        self.snapshot_interval_secs.round() as u64
    }

    /// Get the pool size as an integer.
    #[must_use]
    pub fn pool_size_usize(&self) -> usize {
        self.pool_size.round() as usize
    }
}

// =============================================================================
// Tuning targets
// =============================================================================

/// Target operating points for the control loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningTargets {
    /// Target RSS as fraction of available memory (0.0–1.0).
    pub target_rss_fraction: f64,
    /// Target mux response latency (ms).
    pub target_latency_ms: f64,
    /// Target CPU utilization fraction (0.0–1.0).
    pub target_cpu_fraction: f64,
}

impl Default for TuningTargets {
    fn default() -> Self {
        Self {
            target_rss_fraction: 0.5,
            target_latency_ms: 10.0,
            target_cpu_fraction: 0.3,
        }
    }
}

// =============================================================================
// System metrics input
// =============================================================================

/// System metrics observed at each tick of the control loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunerMetrics {
    /// RSS as fraction of total system memory (0.0–1.0).
    pub rss_fraction: f64,
    /// Mux response latency (ms).
    pub mux_latency_ms: f64,
    /// CPU utilization fraction (0.0–1.0).
    pub cpu_fraction: f64,
}

// =============================================================================
// Bounded candidate evaluation
// =============================================================================

/// Registry key for knobs that the bounded candidate engine is allowed to explore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TunableKnobId {
    /// Runtime ingest/capture output coalescing window.
    #[serde(rename = "runtime.output_coalesce_window_ms")]
    RuntimeOutputCoalesceWindowMs,
    /// Runtime ingest/capture maximum output coalescing delay.
    #[serde(rename = "runtime.output_coalesce_max_delay_ms")]
    RuntimeOutputCoalesceMaxDelayMs,
    /// Runtime ingest/storage maximum coalesced output bytes.
    #[serde(rename = "runtime.output_coalesce_max_bytes")]
    RuntimeOutputCoalesceMaxBytes,
    /// Runtime telemetry percentile sample window.
    #[serde(rename = "runtime.telemetry_percentile_window")]
    RuntimeTelemetryPercentileWindow,
    /// Runtime memory-tier cursor snapshot warning threshold.
    #[serde(rename = "runtime.cursor_snapshot_memory_warn_bytes")]
    RuntimeCursorSnapshotMemoryWarnBytes,
    /// Backpressure warning ratio.
    #[serde(rename = "backpressure.warn_ratio")]
    BackpressureWarnRatio,
    /// Snapshot bridge trigger tick interval.
    #[serde(rename = "snapshot.trigger_bridge_tick_secs")]
    SnapshotTriggerBridgeTickSecs,
    /// Snapshot memory-trigger cooldown.
    #[serde(rename = "snapshot.memory_trigger_cooldown_secs")]
    SnapshotMemoryTriggerCooldownSecs,
    /// Ingest maximum persisted segment size.
    #[serde(rename = "ingest.max_persist_segment_bytes")]
    IngestMaxPersistSegmentBytes,
    /// Pattern dedupe maximum seen-key budget.
    #[serde(rename = "patterns.max_seen_keys")]
    PatternsMaxSeenKeys,
    /// Pattern matching retained tail size.
    #[serde(rename = "patterns.max_tail_size_bytes")]
    PatternsMaxTailSizeBytes,
    /// Pattern Bloom filter false-positive rate.
    #[serde(rename = "patterns.bloom_false_positive_rate")]
    PatternsBloomFalsePositiveRate,
    /// Policy rate-limiter maximum tracked pane count.
    #[serde(rename = "policy.max_tracked_panes")]
    PolicyMaxTrackedPanes,
    /// Policy rate-limiter maximum events retained per pane.
    #[serde(rename = "policy.max_events_per_pane")]
    PolicyMaxEventsPerPane,
    /// Policy cost-tracker maximum pane count.
    #[serde(rename = "policy.cost_tracker_max_panes")]
    PolicyCostTrackerMaxPanes,
    /// Web/API default streaming frequency.
    #[serde(rename = "web.stream_default_max_hz")]
    WebStreamDefaultMaxHz,
    /// Web/API stream catch-up scan limit.
    #[serde(rename = "web.stream_scan_limit")]
    WebStreamScanLimit,
    /// Workflow/CASS handler timeout.
    #[serde(rename = "workflows.cass_*_timeout_secs")]
    WorkflowsCassTimeoutSecs,
    /// Workflow handler cooldown.
    #[serde(rename = "workflows.*_cooldown_ms")]
    WorkflowsCooldownMs,
    /// Search Tantivy writer memory budget.
    #[serde(rename = "search.tantivy_writer_memory_bytes")]
    SearchTantivyWriterMemoryBytes,
    /// IPC accept polling interval.
    #[serde(rename = "ipc.accept_poll_interval_ms")]
    IpcAcceptPollIntervalMs,
    /// Capacity admission queue defer threshold.
    #[serde(rename = "capacity.queue_defer_threshold")]
    CapacityQueueDeferThreshold,
    /// Capacity admission backlog defer threshold.
    #[serde(rename = "capacity.backlog_defer_threshold")]
    CapacityBacklogDeferThreshold,
    /// Capacity admission queue throttle threshold.
    #[serde(rename = "capacity.throttle_queue_depth")]
    CapacityThrottleQueueDepth,
    /// Capacity admission backlog throttle threshold.
    #[serde(rename = "capacity.throttle_backlog_depth")]
    CapacityThrottleBacklogDepth,
    /// Capacity admission queue shed threshold.
    #[serde(rename = "capacity.shed_queue_depth")]
    CapacityShedQueueDepth,
    /// Capacity admission backlog shed threshold.
    #[serde(rename = "capacity.shed_backlog_depth")]
    CapacityShedBacklogDepth,
    /// Capacity admission default retry-after seconds.
    #[serde(rename = "capacity.default_retry_after_secs")]
    CapacityDefaultRetryAfterSecs,
    /// Capacity admission cooldown seconds.
    #[serde(rename = "capacity.cooldown_secs")]
    CapacityCooldownSecs,
}

impl TunableKnobId {
    /// Stable registry id used in decision records.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RuntimeOutputCoalesceWindowMs => "runtime.output_coalesce_window_ms",
            Self::RuntimeOutputCoalesceMaxDelayMs => "runtime.output_coalesce_max_delay_ms",
            Self::RuntimeOutputCoalesceMaxBytes => "runtime.output_coalesce_max_bytes",
            Self::RuntimeTelemetryPercentileWindow => "runtime.telemetry_percentile_window",
            Self::RuntimeCursorSnapshotMemoryWarnBytes => {
                "runtime.cursor_snapshot_memory_warn_bytes"
            }
            Self::BackpressureWarnRatio => "backpressure.warn_ratio",
            Self::SnapshotTriggerBridgeTickSecs => "snapshot.trigger_bridge_tick_secs",
            Self::SnapshotMemoryTriggerCooldownSecs => "snapshot.memory_trigger_cooldown_secs",
            Self::IngestMaxPersistSegmentBytes => "ingest.max_persist_segment_bytes",
            Self::PatternsMaxSeenKeys => "patterns.max_seen_keys",
            Self::PatternsMaxTailSizeBytes => "patterns.max_tail_size_bytes",
            Self::PatternsBloomFalsePositiveRate => "patterns.bloom_false_positive_rate",
            Self::PolicyMaxTrackedPanes => "policy.max_tracked_panes",
            Self::PolicyMaxEventsPerPane => "policy.max_events_per_pane",
            Self::PolicyCostTrackerMaxPanes => "policy.cost_tracker_max_panes",
            Self::WebStreamDefaultMaxHz => "web.stream_default_max_hz",
            Self::WebStreamScanLimit => "web.stream_scan_limit",
            Self::WorkflowsCassTimeoutSecs => "workflows.cass_*_timeout_secs",
            Self::WorkflowsCooldownMs => "workflows.*_cooldown_ms",
            Self::SearchTantivyWriterMemoryBytes => "search.tantivy_writer_memory_bytes",
            Self::IpcAcceptPollIntervalMs => "ipc.accept_poll_interval_ms",
            Self::CapacityQueueDeferThreshold => "capacity.queue_defer_threshold",
            Self::CapacityBacklogDeferThreshold => "capacity.backlog_defer_threshold",
            Self::CapacityThrottleQueueDepth => "capacity.throttle_queue_depth",
            Self::CapacityThrottleBacklogDepth => "capacity.throttle_backlog_depth",
            Self::CapacityShedQueueDepth => "capacity.shed_queue_depth",
            Self::CapacityShedBacklogDepth => "capacity.shed_backlog_depth",
            Self::CapacityDefaultRetryAfterSecs => "capacity.default_retry_after_secs",
            Self::CapacityCooldownSecs => "capacity.cooldown_secs",
        }
    }

    /// Parse a stable registry id.
    #[must_use]
    pub fn from_registry_id(id: &str) -> Option<Self> {
        match id {
            "runtime.output_coalesce_window_ms" => Some(Self::RuntimeOutputCoalesceWindowMs),
            "runtime.output_coalesce_max_delay_ms" => Some(Self::RuntimeOutputCoalesceMaxDelayMs),
            "runtime.output_coalesce_max_bytes" => Some(Self::RuntimeOutputCoalesceMaxBytes),
            "runtime.telemetry_percentile_window" => Some(Self::RuntimeTelemetryPercentileWindow),
            "runtime.cursor_snapshot_memory_warn_bytes" => {
                Some(Self::RuntimeCursorSnapshotMemoryWarnBytes)
            }
            "backpressure.warn_ratio" => Some(Self::BackpressureWarnRatio),
            "snapshot.trigger_bridge_tick_secs" => Some(Self::SnapshotTriggerBridgeTickSecs),
            "snapshot.memory_trigger_cooldown_secs" => {
                Some(Self::SnapshotMemoryTriggerCooldownSecs)
            }
            "ingest.max_persist_segment_bytes" => Some(Self::IngestMaxPersistSegmentBytes),
            "patterns.max_seen_keys" => Some(Self::PatternsMaxSeenKeys),
            "patterns.max_tail_size_bytes" => Some(Self::PatternsMaxTailSizeBytes),
            "patterns.bloom_false_positive_rate" => Some(Self::PatternsBloomFalsePositiveRate),
            "policy.max_tracked_panes" => Some(Self::PolicyMaxTrackedPanes),
            "policy.max_events_per_pane" => Some(Self::PolicyMaxEventsPerPane),
            "policy.cost_tracker_max_panes" => Some(Self::PolicyCostTrackerMaxPanes),
            "web.stream_default_max_hz" => Some(Self::WebStreamDefaultMaxHz),
            "web.stream_scan_limit" => Some(Self::WebStreamScanLimit),
            "workflows.cass_*_timeout_secs" => Some(Self::WorkflowsCassTimeoutSecs),
            "workflows.*_cooldown_ms" => Some(Self::WorkflowsCooldownMs),
            "search.tantivy_writer_memory_bytes" => Some(Self::SearchTantivyWriterMemoryBytes),
            "ipc.accept_poll_interval_ms" => Some(Self::IpcAcceptPollIntervalMs),
            "capacity.queue_defer_threshold" => Some(Self::CapacityQueueDeferThreshold),
            "capacity.backlog_defer_threshold" => Some(Self::CapacityBacklogDeferThreshold),
            "capacity.throttle_queue_depth" => Some(Self::CapacityThrottleQueueDepth),
            "capacity.throttle_backlog_depth" => Some(Self::CapacityThrottleBacklogDepth),
            "capacity.shed_queue_depth" => Some(Self::CapacityShedQueueDepth),
            "capacity.shed_backlog_depth" => Some(Self::CapacityShedBacklogDepth),
            "capacity.default_retry_after_secs" => Some(Self::CapacityDefaultRetryAfterSecs),
            "capacity.cooldown_secs" => Some(Self::CapacityCooldownSecs),
            _ => None,
        }
    }
}

/// Controller mode for bounded candidate evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuningMode {
    /// Compile-time and runtime off state.
    Disabled,
    /// Collect telemetry and emit would-have-tuned decisions.
    Observe,
    /// Evaluate a bounded candidate against a small canary scope.
    Canary,
    /// Try one registry-approved step while tracking regression metrics.
    Exploration,
    /// Keep a previously proven setting inside a narrow range.
    SteadyState,
    /// Restore the last safe value after regression.
    Rollback,
    /// Pause adaptation after rollback, drift, or missing telemetry.
    Cooldown,
}

impl TuningMode {
    /// Stable mode label for decision logs and cockpit rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Observe => "observe",
            Self::Canary => "canary",
            Self::Exploration => "exploration",
            Self::SteadyState => "steady_state",
            Self::Rollback => "rollback",
            Self::Cooldown => "cooldown",
        }
    }

    /// Whether a candidate in this mode may be applied somewhere.
    #[must_use]
    pub const fn would_apply_candidate(self) -> bool {
        matches!(
            self,
            Self::Canary | Self::Exploration | Self::SteadyState | Self::Rollback
        )
    }

    /// Whether this mode is allowed to mutate live knobs.
    #[must_use]
    pub const fn may_mutate_live_knobs(self) -> bool {
        matches!(self, Self::SteadyState | Self::Rollback)
    }

    /// Whether this mode consumes the concurrent exploration budget.
    #[must_use]
    pub const fn consumes_exploration_budget(self) -> bool {
        matches!(self, Self::Canary | Self::Exploration)
    }

    /// Whether the transition is allowed by the ft-luq3w.1 controller contract.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (_, Self::Disabled) => true,
            (Self::Disabled, Self::Observe) => true,
            (Self::Disabled, _) => false,
            (Self::Observe, _) => matches!(next, Self::Observe | Self::Canary),
            (Self::Canary, _) => matches!(
                next,
                Self::Canary | Self::Exploration | Self::Rollback | Self::Cooldown
            ),
            (Self::Exploration, _) => matches!(
                next,
                Self::Exploration | Self::SteadyState | Self::Rollback | Self::Cooldown
            ),
            (Self::SteadyState, _) => {
                matches!(next, Self::SteadyState | Self::Rollback | Self::Cooldown)
            }
            (Self::Rollback, _) => matches!(next, Self::Rollback | Self::Cooldown),
            (Self::Cooldown, _) => matches!(next, Self::Cooldown | Self::Observe | Self::Rollback),
        }
    }
}

/// Trust state for the telemetry window used to evaluate candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryTrust {
    /// Telemetry is fresh enough for the current evaluation window.
    Fresh,
    /// Required telemetry is absent.
    Missing,
    /// Telemetry exists but is too old for the current window.
    Stale,
    /// Telemetry exists but failed a trust or provenance check.
    Untrusted,
}

/// Direction for one bounded knob step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateDirection {
    /// Move the value upward.
    Increase,
    /// Move the value downward.
    Decrease,
}

/// Step policy for one bounded candidate move.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStep {
    /// Add or subtract a fixed amount.
    Fixed { amount: f64 },
    /// Add or subtract a fraction of the current value.
    Fraction { fraction: f64 },
    /// Multiply by this factor when increasing, divide by it when decreasing.
    Multiplier { factor: f64 },
    /// Move by one documented profile tier.
    Tier { amount: f64 },
}

/// Registry gate required before a candidate can leave observe mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnobGate {
    /// Requires an explicit operator profile gate.
    ProfileGated,
    /// Must emit observe decisions before any applied candidate.
    ObserveFirst,
    /// Must start in a canary scope.
    CanaryFirst,
}

/// Static registry row for one safe tunable knob.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KnobSpec {
    /// Stable registry id.
    pub id: TunableKnobId,
    /// Owning subsystem.
    pub owner: &'static str,
    /// Current configuration source.
    pub source: &'static str,
    /// Default value from the safe tuning contract.
    pub default_value: f64,
    /// Hard range for generated candidates.
    pub range: ParamRange,
    /// Initial bounded step policy.
    pub step: CandidateStep,
    /// Primary safety metric for this knob.
    pub safety_metric: &'static str,
    /// Primary rollback metric for this knob.
    pub rollback_metric: &'static str,
    /// Dynamic or cross-field constraint that cannot be represented by min/max alone.
    pub constraint: &'static str,
    /// Gate required by the registry.
    pub gate: KnobGate,
}

/// Bounded candidate generator configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEvaluationConfig {
    /// Current controller mode.
    pub mode: TuningMode,
    /// Maximum number of concurrent exploration candidates.
    pub max_concurrent_explorations: usize,
    /// Freshness and trust state for the telemetry window.
    pub telemetry_trust: TelemetryTrust,
}

impl Default for CandidateEvaluationConfig {
    fn default() -> Self {
        Self {
            mode: TuningMode::Observe,
            max_concurrent_explorations: 1,
            telemetry_trust: TelemetryTrust::Fresh,
        }
    }
}

/// Telemetry window evaluated for one candidate pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateTelemetryWindow {
    /// Whether the warmup portion of the window has completed.
    pub warmup_complete: bool,
    /// Number of measurements in the current window.
    pub measurement_count: usize,
    /// Minimum required measurements for this window.
    pub minimum_measurements: usize,
    /// Confidence score for the measured pressure signal.
    pub confidence: f64,
    /// Minimum confidence required before a candidate may be emitted.
    pub minimum_confidence: f64,
    /// Direction recommended by trusted telemetry for this candidate window.
    pub direction: Option<CandidateDirection>,
}

impl Default for CandidateTelemetryWindow {
    fn default() -> Self {
        Self {
            warmup_complete: true,
            measurement_count: 30,
            minimum_measurements: 10,
            confidence: 0.95,
            minimum_confidence: 0.80,
            direction: None,
        }
    }
}

/// Why a candidate was not generated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateSkipReason {
    /// The controller is disabled.
    Disabled,
    /// The controller is in cooldown.
    Cooldown,
    /// Required telemetry is missing.
    MissingTelemetry,
    /// Required telemetry is stale.
    StaleTelemetry,
    /// Required telemetry failed a trust check.
    UntrustedTelemetry,
    /// Telemetry warmup has not completed.
    WarmupIncomplete,
    /// Telemetry window has too few measurements.
    InsufficientMeasurements,
    /// Telemetry confidence is below threshold.
    InsufficientConfidence,
    /// The concurrent exploration budget is exhausted.
    ExplorationBudgetExhausted,
    /// The requested knob is not in the safe registry.
    UnknownKnob,
    /// The first implementation forbids multi-knob candidates.
    MultipleKnobsForbidden,
    /// Requested knobs violate an explicit combination guardrail.
    UnsafeCombination,
    /// The requested knob is pinned by the operator.
    PinnedKnob,
    /// Current value for the requested knob was not available.
    MissingCurrentValue,
    /// Current value is outside hard bounds or is not finite.
    InvalidCurrentValue,
    /// There is no pressure signal for any unpinned safe knob.
    NoPressureSignal,
}

impl CandidateSkipReason {
    /// Stable reason code for decision logs and cockpit rows.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::Disabled => "auto_tune.skipped.disabled",
            Self::Cooldown => "auto_tune.skipped.cooldown",
            Self::MissingTelemetry => "auto_tune.skipped.missing_telemetry",
            Self::StaleTelemetry => "auto_tune.skipped.stale_telemetry",
            Self::UntrustedTelemetry => "auto_tune.skipped.untrusted_telemetry",
            Self::WarmupIncomplete => "auto_tune.skipped.warmup_incomplete",
            Self::InsufficientMeasurements => "auto_tune.skipped.insufficient_measurements",
            Self::InsufficientConfidence => "auto_tune.skipped.insufficient_confidence",
            Self::ExplorationBudgetExhausted => "auto_tune.skipped.exploration_budget_exhausted",
            Self::UnknownKnob => "auto_tune.skipped.unknown_knob",
            Self::MultipleKnobsForbidden => "auto_tune.skipped.multiple_knobs_forbidden",
            Self::UnsafeCombination => "auto_tune.skipped.unsafe_combination",
            Self::PinnedKnob => "auto_tune.skipped.pinned_knob",
            Self::MissingCurrentValue => "auto_tune.skipped.missing_current_value",
            Self::InvalidCurrentValue => "auto_tune.skipped.invalid_current_value",
            Self::NoPressureSignal => "auto_tune.skipped.no_pressure_signal",
        }
    }
}

/// Candidate for one bounded registry-approved knob step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningCandidate {
    /// Registry id.
    pub knob_id: TunableKnobId,
    /// Owning subsystem.
    pub owner: String,
    /// Previous value.
    pub old_value: f64,
    /// Bounded candidate value.
    pub candidate_value: f64,
    /// Step direction.
    pub direction: CandidateDirection,
    /// Step policy from the registry.
    pub step: CandidateStep,
    /// Whether the current mode permits application somewhere.
    pub would_apply: bool,
    /// Whether the current mode may mutate live knobs.
    pub live_mutation_allowed: bool,
    /// Stable machine-readable reason code.
    pub reason_code: String,
    /// Primary safety metric from the registry.
    pub safety_metric: String,
    /// Primary rollback metric from the registry.
    pub rollback_metric: String,
    /// Registry gate that must be satisfied before widening.
    pub gate: KnobGate,
}

/// Result of one bounded candidate evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateDecision {
    /// Mode used for this evaluation.
    pub mode: TuningMode,
    /// Candidate, when one was safely generated.
    pub candidate: Option<TuningCandidate>,
    /// Reasons candidate generation was skipped.
    pub skip_reasons: Vec<CandidateSkipReason>,
    /// Current active exploration count.
    pub active_explorations: usize,
    /// Configured maximum active exploration count.
    pub max_concurrent_explorations: usize,
}

/// Registry-only, single-knob candidate engine for ft-luq3w.2.
#[derive(Debug, Clone)]
pub struct BoundedCandidateEngine {
    registry: BTreeMap<TunableKnobId, KnobSpec>,
    config: CandidateEvaluationConfig,
    active_explorations: usize,
}

impl BoundedCandidateEngine {
    /// Build an engine with the default safe registry.
    #[must_use]
    pub fn new(config: CandidateEvaluationConfig) -> Self {
        Self {
            registry: default_candidate_registry(),
            config,
            active_explorations: 0,
        }
    }

    /// Return the safe registry.
    #[must_use]
    pub fn registry(&self) -> &BTreeMap<TunableKnobId, KnobSpec> {
        &self.registry
    }

    /// Return the engine config.
    #[must_use]
    pub fn config(&self) -> &CandidateEvaluationConfig {
        &self.config
    }

    /// Set the active exploration count reported by the caller.
    pub fn set_active_explorations(&mut self, active: usize) {
        self.active_explorations = active;
    }

    /// Validate that a textual knob id belongs to the safe registry.
    #[must_use]
    pub fn validate_knob_id(&self, knob_id: &str) -> Result<TunableKnobId, CandidateSkipReason> {
        let Some(id) = TunableKnobId::from_registry_id(knob_id) else {
            return Err(CandidateSkipReason::UnknownKnob);
        };

        if self.registry.contains_key(&id) {
            Ok(id)
        } else {
            Err(CandidateSkipReason::UnknownKnob)
        }
    }

    /// Validate a requested candidate knob set against the initial guardrails.
    pub fn validate_requested_knobs(
        &self,
        knob_ids: &[&str],
    ) -> Result<Vec<TunableKnobId>, Vec<CandidateSkipReason>> {
        let mut skip_reasons = Vec::new();
        let mut ids = Vec::with_capacity(knob_ids.len());

        for knob_id in knob_ids {
            match self.validate_knob_id(knob_id) {
                Ok(id) => ids.push(id),
                Err(reason) => skip_reasons.push(reason),
            }
        }

        if ids.len() > 1 {
            skip_reasons.push(CandidateSkipReason::MultipleKnobsForbidden);
            skip_reasons.push(CandidateSkipReason::UnsafeCombination);
        }

        if skip_reasons.is_empty() {
            Ok(ids)
        } else {
            Err(skip_reasons)
        }
    }

    /// Evaluate the current metrics and return at most one bounded candidate.
    #[must_use]
    pub fn evaluate(
        &self,
        current_values: &BTreeMap<TunableKnobId, f64>,
        telemetry: &CandidateTelemetryWindow,
        pinned: &[TunableKnobId],
    ) -> CandidateDecision {
        let skip_reasons = self.preflight_skip_reasons(telemetry);

        if !skip_reasons.is_empty() {
            return self.skipped(skip_reasons);
        }

        let Some(direction) = telemetry.direction else {
            return self.skipped(vec![CandidateSkipReason::NoPressureSignal]);
        };

        for spec in self.registry.values() {
            if pinned.contains(&spec.id) {
                continue;
            }

            let Some(old_value) = current_values.get(&spec.id).copied() else {
                continue;
            };

            match self.build_candidate(spec, old_value, direction) {
                Ok(candidate) => {
                    return CandidateDecision {
                        mode: self.config.mode,
                        candidate: Some(candidate),
                        skip_reasons,
                        active_explorations: self.active_explorations,
                        max_concurrent_explorations: self.config.max_concurrent_explorations,
                    };
                }
                Err(CandidateSkipReason::NoPressureSignal) => continue,
                Err(reason) => return self.skipped(vec![reason]),
            }
        }

        self.skipped(vec![CandidateSkipReason::NoPressureSignal])
    }

    /// Evaluate a caller-requested knob set after validating registry and combination guards.
    #[must_use]
    pub fn evaluate_requested(
        &self,
        knob_ids: &[&str],
        current_values: &BTreeMap<TunableKnobId, f64>,
        telemetry: &CandidateTelemetryWindow,
        pinned: &[TunableKnobId],
    ) -> CandidateDecision {
        let mut skip_reasons = self.preflight_skip_reasons(telemetry);

        let requested = match self.validate_requested_knobs(knob_ids) {
            Ok(requested) => requested,
            Err(mut reasons) => {
                skip_reasons.append(&mut reasons);
                return self.skipped(skip_reasons);
            }
        };

        if !skip_reasons.is_empty() {
            return self.skipped(skip_reasons);
        }

        if requested.is_empty() {
            return self.evaluate(current_values, telemetry, pinned);
        }

        let knob_id = requested[0];
        let Some(spec) = self.registry.get(&knob_id) else {
            return self.skipped(vec![CandidateSkipReason::UnknownKnob]);
        };

        if pinned.contains(&knob_id) {
            return self.skipped(vec![CandidateSkipReason::PinnedKnob]);
        }

        let Some(old_value) = current_values.get(&knob_id).copied() else {
            return self.skipped(vec![CandidateSkipReason::MissingCurrentValue]);
        };

        let Some(direction) = telemetry.direction else {
            return self.skipped(vec![CandidateSkipReason::NoPressureSignal]);
        };

        match self.build_candidate(spec, old_value, direction) {
            Ok(candidate) => CandidateDecision {
                mode: self.config.mode,
                candidate: Some(candidate),
                skip_reasons,
                active_explorations: self.active_explorations,
                max_concurrent_explorations: self.config.max_concurrent_explorations,
            },
            Err(reason) => self.skipped(vec![reason]),
        }
    }

    fn preflight_skip_reasons(
        &self,
        telemetry: &CandidateTelemetryWindow,
    ) -> Vec<CandidateSkipReason> {
        let mut skip_reasons = Vec::new();

        match self.config.mode {
            TuningMode::Disabled => skip_reasons.push(CandidateSkipReason::Disabled),
            TuningMode::Cooldown => skip_reasons.push(CandidateSkipReason::Cooldown),
            _ => {}
        }

        match self.config.telemetry_trust {
            TelemetryTrust::Fresh => {}
            TelemetryTrust::Missing => skip_reasons.push(CandidateSkipReason::MissingTelemetry),
            TelemetryTrust::Stale => skip_reasons.push(CandidateSkipReason::StaleTelemetry),
            TelemetryTrust::Untrusted => skip_reasons.push(CandidateSkipReason::UntrustedTelemetry),
        }

        if !telemetry.warmup_complete {
            skip_reasons.push(CandidateSkipReason::WarmupIncomplete);
        }
        if telemetry.measurement_count < telemetry.minimum_measurements {
            skip_reasons.push(CandidateSkipReason::InsufficientMeasurements);
        }
        if telemetry.confidence < telemetry.minimum_confidence {
            skip_reasons.push(CandidateSkipReason::InsufficientConfidence);
        }
        if self.config.mode.consumes_exploration_budget()
            && self.active_explorations >= self.config.max_concurrent_explorations
        {
            skip_reasons.push(CandidateSkipReason::ExplorationBudgetExhausted);
        }

        skip_reasons
    }

    fn build_candidate(
        &self,
        spec: &KnobSpec,
        old_value: f64,
        direction: CandidateDirection,
    ) -> Result<TuningCandidate, CandidateSkipReason> {
        if !spec.range.contains(old_value) {
            return Err(CandidateSkipReason::InvalidCurrentValue);
        }

        let Some(candidate_value) = bounded_step(old_value, spec.range, spec.step, direction)
        else {
            return Err(CandidateSkipReason::InvalidCurrentValue);
        };

        if (candidate_value - old_value).abs() <= f64::EPSILON {
            return Err(CandidateSkipReason::NoPressureSignal);
        }

        Ok(TuningCandidate {
            knob_id: spec.id,
            owner: spec.owner.to_string(),
            old_value,
            candidate_value,
            direction,
            step: spec.step,
            would_apply: self.config.mode.would_apply_candidate(),
            live_mutation_allowed: self.config.mode.may_mutate_live_knobs(),
            reason_code: format!("auto_tune.candidate.{}", spec.id.as_str()),
            safety_metric: spec.safety_metric.to_string(),
            rollback_metric: spec.rollback_metric.to_string(),
            gate: spec.gate,
        })
    }

    fn skipped(&self, skip_reasons: Vec<CandidateSkipReason>) -> CandidateDecision {
        CandidateDecision {
            mode: self.config.mode,
            candidate: None,
            skip_reasons,
            active_explorations: self.active_explorations,
            max_concurrent_explorations: self.config.max_concurrent_explorations,
        }
    }
}

fn default_candidate_registry() -> BTreeMap<TunableKnobId, KnobSpec> {
    use CandidateStep::{Fixed, Multiplier, Tier};
    use KnobGate::{CanaryFirst, ObserveFirst, ProfileGated};

    BTreeMap::from([
        (
            TunableKnobId::RuntimeOutputCoalesceWindowMs,
            KnobSpec {
                id: TunableKnobId::RuntimeOutputCoalesceWindowMs,
                owner: "runtime",
                source: "RuntimeTuning::output_coalesce_window_ms",
                default_value: 50.0,
                range: ParamRange {
                    min: 5.0,
                    max: 200.0,
                },
                step: Fixed { amount: 25.0 },
                safety_metric: "ingest_p95_latency",
                rollback_metric: "p99_ingest_latency",
                constraint: "segment_flush_count and buffered memory must remain in budget",
                gate: ProfileGated,
            },
        ),
        (
            TunableKnobId::RuntimeOutputCoalesceMaxDelayMs,
            KnobSpec {
                id: TunableKnobId::RuntimeOutputCoalesceMaxDelayMs,
                owner: "runtime",
                source: "RuntimeTuning::output_coalesce_max_delay_ms",
                default_value: 200.0,
                range: ParamRange {
                    min: 5.0,
                    max: 750.0,
                },
                step: Fixed { amount: 50.0 },
                safety_metric: "flush_boundedness",
                rollback_metric: "pane_freshness_p99",
                constraint: "candidate must remain >= runtime.output_coalesce_window_ms",
                gate: ProfileGated,
            },
        ),
        (
            TunableKnobId::RuntimeOutputCoalesceMaxBytes,
            KnobSpec {
                id: TunableKnobId::RuntimeOutputCoalesceMaxBytes,
                owner: "runtime",
                source: "RuntimeTuning::output_coalesce_max_bytes",
                default_value: 262_144.0,
                range: ParamRange {
                    min: 4096.0,
                    max: 1_048_576.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "storage_write_service_p95",
                rollback_metric: "storage_queue_depth",
                constraint: "one x2 or /2 step only",
                gate: ProfileGated,
            },
        ),
        (
            TunableKnobId::RuntimeTelemetryPercentileWindow,
            KnobSpec {
                id: TunableKnobId::RuntimeTelemetryPercentileWindow,
                owner: "runtime",
                source: "RuntimeTuning::telemetry_percentile_window",
                default_value: 1024.0,
                range: ParamRange {
                    min: 256.0,
                    max: 4096.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "telemetry_memory_cap",
                rollback_metric: "stage_sample_contention",
                constraint: "sample sufficiency must remain in budget",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::RuntimeCursorSnapshotMemoryWarnBytes,
            KnobSpec {
                id: TunableKnobId::RuntimeCursorSnapshotMemoryWarnBytes,
                owner: "runtime",
                source: "RuntimeTuning::cursor_snapshot_memory_warn_bytes",
                default_value: 67_108_864.0,
                range: ParamRange {
                    min: 33_554_432.0,
                    max: 536_870_912.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "memory_pressure_tier",
                rollback_metric: "hot_resident_bytes",
                constraint: "retained cursor bytes must remain in budget",
                gate: ProfileGated,
            },
        ),
        (
            TunableKnobId::BackpressureWarnRatio,
            KnobSpec {
                id: TunableKnobId::BackpressureWarnRatio,
                owner: "backpressure",
                source: "BackpressureTuning::warn_ratio",
                default_value: 0.75,
                range: ParamRange {
                    min: 0.10,
                    max: 0.99,
                },
                step: Fixed { amount: 0.05 },
                safety_metric: "false_positive_warning_rate",
                rollback_metric: "queue_saturation_rate",
                constraint: "warning timing must precede capacity action",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::SnapshotTriggerBridgeTickSecs,
            KnobSpec {
                id: TunableKnobId::SnapshotTriggerBridgeTickSecs,
                owner: "snapshot",
                source: "SnapshotTuning::trigger_bridge_tick_secs",
                default_value: 30.0,
                range: ParamRange {
                    min: 5.0,
                    max: 120.0,
                },
                step: Fixed { amount: 15.0 },
                safety_metric: "snapshot_trigger_latency",
                rollback_metric: "idle_cpu",
                constraint: "must not miss trigger SLA",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::SnapshotMemoryTriggerCooldownSecs,
            KnobSpec {
                id: TunableKnobId::SnapshotMemoryTriggerCooldownSecs,
                owner: "snapshot",
                source: "SnapshotTuning::memory_trigger_cooldown_secs",
                default_value: 120.0,
                range: ParamRange {
                    min: 60.0,
                    max: 600.0,
                },
                step: Fixed { amount: 60.0 },
                safety_metric: "repeated_memory_trigger_rate",
                rollback_metric: "snapshot_io_pressure",
                constraint: "memory recovery must stay inside budget",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::IngestMaxPersistSegmentBytes,
            KnobSpec {
                id: TunableKnobId::IngestMaxPersistSegmentBytes,
                owner: "ingest",
                source: "IngestTuning::max_persist_segment_bytes",
                default_value: 65_536.0,
                range: ParamRange {
                    min: 32_768.0,
                    max: 262_144.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "storage_write_p95",
                rollback_metric: "search_freshness_lag",
                constraint: "memory queue must not exceed baseline budget",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::PatternsMaxSeenKeys,
            KnobSpec {
                id: TunableKnobId::PatternsMaxSeenKeys,
                owner: "patterns",
                source: "PatternsTuning::max_seen_keys",
                default_value: 1000.0,
                range: ParamRange {
                    min: 100.0,
                    max: 64_000.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "dedupe_hit_rate",
                rollback_metric: "pattern_cpu",
                constraint: "memory footprint must remain in budget",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::PatternsMaxTailSizeBytes,
            KnobSpec {
                id: TunableKnobId::PatternsMaxTailSizeBytes,
                owner: "patterns",
                source: "PatternsTuning::max_tail_size_bytes",
                default_value: 2048.0,
                range: ParamRange {
                    min: 256.0,
                    max: 16_384.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "detection_recall_proxy",
                rollback_metric: "regex_cpu",
                constraint: "retained tail bytes must remain in budget",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::PatternsBloomFalsePositiveRate,
            KnobSpec {
                id: TunableKnobId::PatternsBloomFalsePositiveRate,
                owner: "patterns",
                source: "PatternsTuning::bloom_false_positive_rate",
                default_value: 0.01,
                range: ParamRange {
                    min: 0.001,
                    max: 0.2,
                },
                step: Tier { amount: 0.009 },
                safety_metric: "regex_evaluations",
                rollback_metric: "false_positive_work",
                constraint: "one documented tier only",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::PolicyMaxTrackedPanes,
            KnobSpec {
                id: TunableKnobId::PolicyMaxTrackedPanes,
                owner: "policy",
                source: "PolicyTuning::max_tracked_panes",
                default_value: 256.0,
                range: ParamRange {
                    min: 32.0,
                    max: 8192.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "rate_limit_amnesia_count",
                rollback_metric: "policy_memory",
                constraint: "evictions must not remain high",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::PolicyMaxEventsPerPane,
            KnobSpec {
                id: TunableKnobId::PolicyMaxEventsPerPane,
                owner: "policy",
                source: "PolicyTuning::max_events_per_pane",
                default_value: 64.0,
                range: ParamRange {
                    min: 8.0,
                    max: 512.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "rate_limit_accuracy",
                rollback_metric: "enforcement_churn",
                constraint: "policy memory must remain in budget",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::PolicyCostTrackerMaxPanes,
            KnobSpec {
                id: TunableKnobId::PolicyCostTrackerMaxPanes,
                owner: "policy",
                source: "PolicyTuning::cost_tracker_max_panes",
                default_value: 512.0,
                range: ParamRange {
                    min: 128.0,
                    max: 8192.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "cost_tracker_eviction_count",
                rollback_metric: "cost_tracker_memory",
                constraint: "cost tracker must stop evicting after increase",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::WebStreamDefaultMaxHz,
            KnobSpec {
                id: TunableKnobId::WebStreamDefaultMaxHz,
                owner: "web",
                source: "WebTuning::stream_default_max_hz",
                default_value: 50.0,
                range: ParamRange {
                    min: 1.0,
                    max: 250.0,
                },
                step: Tier { amount: 25.0 },
                safety_metric: "sse_lag",
                rollback_metric: "client_backlog",
                constraint: "CPU fanout must remain inside baseline",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::WebStreamScanLimit,
            KnobSpec {
                id: TunableKnobId::WebStreamScanLimit,
                owner: "web",
                source: "WebTuning::stream_scan_limit",
                default_value: 256.0,
                range: ParamRange {
                    min: 1.0,
                    max: 1024.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "catch_up_latency",
                rollback_metric: "request_latency",
                constraint: "scan CPU must not regress",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::WorkflowsCassTimeoutSecs,
            KnobSpec {
                id: TunableKnobId::WorkflowsCassTimeoutSecs,
                owner: "workflows",
                source: "CassQueryConfig::timeout_secs",
                default_value: 6.0,
                range: ParamRange {
                    min: 4.0,
                    max: 15.0,
                },
                step: Fixed { amount: 2.0 },
                safety_metric: "cass_success_rate",
                rollback_metric: "workflow_p99",
                constraint: "cancellation rate must not regress",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::WorkflowsCooldownMs,
            KnobSpec {
                id: TunableKnobId::WorkflowsCooldownMs,
                owner: "workflows",
                source: "WorkflowsTuning::*_cooldown_ms",
                default_value: 180_000.0,
                range: ParamRange {
                    min: 60_000.0,
                    max: 1_800_000.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "duplicate_automation_rate",
                rollback_metric: "recovery_latency",
                constraint: "repeated intervention must not regress",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::SearchTantivyWriterMemoryBytes,
            KnobSpec {
                id: TunableKnobId::SearchTantivyWriterMemoryBytes,
                owner: "search",
                source: "SearchTuning::tantivy_writer_memory_bytes",
                default_value: 50_000_000.0,
                range: ParamRange {
                    min: 10_485_760.0,
                    max: 268_435_456.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "indexing_throughput",
                rollback_metric: "search_lag",
                constraint: "memory pressure must not regress",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::IpcAcceptPollIntervalMs,
            KnobSpec {
                id: TunableKnobId::IpcAcceptPollIntervalMs,
                owner: "ipc",
                source: "IpcTuning::accept_poll_interval_ms",
                default_value: 100.0,
                range: ParamRange {
                    min: 10.0,
                    max: 250.0,
                },
                step: Fixed { amount: 25.0 },
                safety_metric: "accept_latency",
                rollback_metric: "idle_cpu",
                constraint: "accept p99 must not regress",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::CapacityQueueDeferThreshold,
            KnobSpec {
                id: TunableKnobId::CapacityQueueDeferThreshold,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::queue_defer_threshold",
                default_value: 16.0,
                range: ParamRange {
                    min: 1.0,
                    max: 1024.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "admission_queue_pressure",
                rollback_metric: "defer_count",
                constraint: "defer <= throttle <= shed",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::CapacityBacklogDeferThreshold,
            KnobSpec {
                id: TunableKnobId::CapacityBacklogDeferThreshold,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::backlog_defer_threshold",
                default_value: 64.0,
                range: ParamRange {
                    min: 1.0,
                    max: 4096.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "admission_backlog_pressure",
                rollback_metric: "backlog_defer_count",
                constraint: "defer <= throttle <= shed",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::CapacityThrottleQueueDepth,
            KnobSpec {
                id: TunableKnobId::CapacityThrottleQueueDepth,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::throttle_queue_depth",
                default_value: 64.0,
                range: ParamRange {
                    min: 1.0,
                    max: 4096.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "queue_throttle_rate",
                rollback_metric: "capture_freshness",
                constraint: "queue_defer_threshold <= candidate <= shed_queue_depth",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::CapacityThrottleBacklogDepth,
            KnobSpec {
                id: TunableKnobId::CapacityThrottleBacklogDepth,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::throttle_backlog_depth",
                default_value: 256.0,
                range: ParamRange {
                    min: 1.0,
                    max: 16_384.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "backlog_throttle_rate",
                rollback_metric: "backlog_drain_time",
                constraint: "backlog_defer_threshold <= candidate <= shed_backlog_depth",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::CapacityShedQueueDepth,
            KnobSpec {
                id: TunableKnobId::CapacityShedQueueDepth,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::shed_queue_depth",
                default_value: 256.0,
                range: ParamRange {
                    min: 1.0,
                    max: 4096.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "optional_shed_count",
                rollback_metric: "queue_saturation",
                constraint: "candidate >= throttle_queue_depth",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::CapacityShedBacklogDepth,
            KnobSpec {
                id: TunableKnobId::CapacityShedBacklogDepth,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::shed_backlog_depth",
                default_value: 1024.0,
                range: ParamRange {
                    min: 1.0,
                    max: 16_384.0,
                },
                step: Multiplier { factor: 2.0 },
                safety_metric: "optional_backlog_shed_count",
                rollback_metric: "backlog_saturation",
                constraint: "candidate >= throttle_backlog_depth",
                gate: CanaryFirst,
            },
        ),
        (
            TunableKnobId::CapacityDefaultRetryAfterSecs,
            KnobSpec {
                id: TunableKnobId::CapacityDefaultRetryAfterSecs,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::default_retry_after_secs",
                default_value: 5.0,
                range: ParamRange {
                    min: 1.0,
                    max: 3600.0,
                },
                step: Fixed { amount: 5.0 },
                safety_metric: "retry_storm_rate",
                rollback_metric: "completion_latency",
                constraint: "candidate <= max_retry_after_secs",
                gate: ObserveFirst,
            },
        ),
        (
            TunableKnobId::CapacityCooldownSecs,
            KnobSpec {
                id: TunableKnobId::CapacityCooldownSecs,
                owner: "capacity",
                source: "SwarmCapacityAdmissionControllerConfig::cooldown_secs",
                default_value: 30.0,
                range: ParamRange {
                    min: 0.0,
                    max: 3600.0,
                },
                step: Fixed { amount: 30.0 },
                safety_metric: "oscillation_count",
                rollback_metric: "recovery_time",
                constraint: "cooldown semantics must match capacity admission",
                gate: ObserveFirst,
            },
        ),
    ])
}

fn bounded_step(
    current: f64,
    range: ParamRange,
    step: CandidateStep,
    direction: CandidateDirection,
) -> Option<f64> {
    if !current.is_finite() || !range.min.is_finite() || !range.max.is_finite() {
        return None;
    }

    let candidate = match (step, direction) {
        (CandidateStep::Fixed { amount } | CandidateStep::Tier { amount }, _) => {
            if !amount.is_finite() || amount <= 0.0 {
                return None;
            }
            match direction {
                CandidateDirection::Increase => current + amount,
                CandidateDirection::Decrease => current - amount,
            }
        }
        (CandidateStep::Fraction { fraction }, _) => {
            if !fraction.is_finite() || fraction <= 0.0 {
                return None;
            }
            let amount = (current.abs() * fraction).max(f64::EPSILON);
            match direction {
                CandidateDirection::Increase => current + amount,
                CandidateDirection::Decrease => current - amount,
            }
        }
        (CandidateStep::Multiplier { factor }, CandidateDirection::Increase) => {
            if !factor.is_finite() || factor <= 1.0 {
                return None;
            }
            current * factor
        }
        (CandidateStep::Multiplier { factor }, CandidateDirection::Decrease) => {
            if !factor.is_finite() || factor <= 1.0 {
                return None;
            }
            current / factor
        }
    };

    if candidate.is_finite() {
        Some(range.clamp(candidate))
    } else {
        None
    }
}

// =============================================================================
// Rollback safety controller
// =============================================================================

/// Safety metric families enforced before an auto-tuned candidate can persist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMetricKind {
    /// Latency percentiles or service times.
    Latency,
    /// Queue or backlog depth.
    QueueDepth,
    /// Resident memory, pressure tier, or memory budget utilization.
    MemoryPressure,
    /// Dropped, shed, or otherwise abandoned work.
    DroppedWork,
    /// Error or failure rate.
    ErrorRate,
    /// Policy denials, approval failures, or rate-limit misses.
    PolicyApprovalFailures,
}

impl SafetyMetricKind {
    /// Stable metric id used in rollback reason codes.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Latency => "latency",
            Self::QueueDepth => "queue_depth",
            Self::MemoryPressure => "memory_pressure",
            Self::DroppedWork => "dropped_work",
            Self::ErrorRate => "error_rate",
            Self::PolicyApprovalFailures => "policy_approval_failures",
        }
    }
}

/// Monotonic direction for a safety metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMetricGoal {
    /// Observed value must remain below or equal to the baseline plus tolerance.
    LowerOrEqual,
    /// Observed value must remain above or equal to the baseline minus tolerance.
    HigherOrEqual,
}

impl SafetyMetricGoal {
    fn limit(self, baseline: f64, max_regression_fraction: f64) -> Option<f64> {
        if !baseline.is_finite()
            || !max_regression_fraction.is_finite()
            || max_regression_fraction < 0.0
        {
            return None;
        }

        match self {
            Self::LowerOrEqual => Some(baseline * (1.0 + max_regression_fraction)),
            Self::HigherOrEqual => Some(baseline * (1.0 - max_regression_fraction)),
        }
    }

    fn is_regressed(self, observed: f64, limit: f64) -> bool {
        match self {
            Self::LowerOrEqual => observed > limit,
            Self::HigherOrEqual => observed < limit,
        }
    }
}

/// One safety metric observed for an active candidate window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyMetricSample {
    /// Metric family being checked.
    pub metric: SafetyMetricKind,
    /// Last known safe baseline.
    pub baseline: Option<f64>,
    /// Observed candidate-window value.
    pub observed: Option<f64>,
    /// Allowed fractional regression from the baseline.
    pub max_regression_fraction: f64,
    /// Whether absence or invalidity must fail the candidate closed.
    pub required: bool,
    /// Monotonic safety direction.
    pub goal: SafetyMetricGoal,
}

/// Per-metric safety verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyMetricVerdict {
    /// Metric is inside its monotonic safety bound.
    Healthy,
    /// Required telemetry was absent.
    MissingTelemetry,
    /// Telemetry existed but could not be trusted as a finite bound.
    InvalidTelemetry,
    /// Metric crossed the configured regression threshold.
    Regressed,
}

/// Machine-readable result for one safety metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyMetricCheck {
    /// Metric family being checked.
    pub metric: SafetyMetricKind,
    /// Per-metric verdict.
    pub verdict: SafetyMetricVerdict,
    /// Last known safe baseline, when available.
    pub baseline: Option<f64>,
    /// Observed candidate-window value, when available.
    pub observed: Option<f64>,
    /// Calculated safety limit, when available.
    pub limit: Option<f64>,
    /// Whether this metric was required for the candidate.
    pub required: bool,
    /// Stable reason code for operator logs and robot/cockpit surfaces.
    pub reason_code: String,
}

impl SafetyMetricCheck {
    fn from_sample(sample: &SafetyMetricSample) -> Self {
        let (Some(baseline), Some(observed)) = (sample.baseline, sample.observed) else {
            let suffix = if sample.required {
                "missing_telemetry"
            } else {
                "optional_missing_telemetry"
            };
            return Self {
                metric: sample.metric,
                verdict: SafetyMetricVerdict::MissingTelemetry,
                baseline: sample.baseline,
                observed: sample.observed,
                limit: None,
                required: sample.required,
                reason_code: metric_reason_code(sample.metric, suffix),
            };
        };

        let limit = sample.goal.limit(baseline, sample.max_regression_fraction);

        let Some(limit) = limit else {
            return Self {
                metric: sample.metric,
                verdict: SafetyMetricVerdict::InvalidTelemetry,
                baseline: sample.baseline,
                observed: sample.observed,
                limit: None,
                required: sample.required,
                reason_code: metric_reason_code(sample.metric, "invalid_telemetry"),
            };
        };

        if !observed.is_finite() {
            return Self {
                metric: sample.metric,
                verdict: SafetyMetricVerdict::InvalidTelemetry,
                baseline: sample.baseline,
                observed: sample.observed,
                limit: Some(limit),
                required: sample.required,
                reason_code: metric_reason_code(sample.metric, "invalid_telemetry"),
            };
        }

        if sample.goal.is_regressed(observed, limit) {
            return Self {
                metric: sample.metric,
                verdict: SafetyMetricVerdict::Regressed,
                baseline: sample.baseline,
                observed: sample.observed,
                limit: Some(limit),
                required: sample.required,
                reason_code: metric_reason_code(sample.metric, "regressed"),
            };
        }

        Self {
            metric: sample.metric,
            verdict: SafetyMetricVerdict::Healthy,
            baseline: sample.baseline,
            observed: sample.observed,
            limit: Some(limit),
            required: sample.required,
            reason_code: metric_reason_code(sample.metric, "healthy"),
        }
    }

    fn should_rollback(&self) -> bool {
        self.required
            && matches!(
                self.verdict,
                SafetyMetricVerdict::MissingTelemetry
                    | SafetyMetricVerdict::InvalidTelemetry
                    | SafetyMetricVerdict::Regressed
            )
    }
}

/// Safety telemetry for one active candidate evaluation window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SafetyTelemetryWindow {
    /// Whether the warmup portion of the window has completed.
    pub warmup_complete: bool,
    /// Confidence score for the safety window.
    pub confidence: f64,
    /// Minimum confidence required before a candidate may persist.
    pub minimum_confidence: f64,
    /// Safety samples observed for this candidate.
    pub samples: Vec<SafetyMetricSample>,
}

impl Default for SafetyTelemetryWindow {
    fn default() -> Self {
        Self {
            warmup_complete: true,
            confidence: 0.95,
            minimum_confidence: 0.80,
            samples: Vec::new(),
        }
    }
}

/// Configuration for rollback hysteresis and cooldown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackControllerConfig {
    /// Whether rollback control is enabled.
    pub enabled: bool,
    /// Consecutive regressed windows required before rollback.
    pub regression_hysteresis_windows: usize,
    /// Candidate windows to pause after rollback.
    pub cooldown_windows: usize,
}

impl Default for RollbackControllerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            regression_hysteresis_windows: 2,
            cooldown_windows: 3,
        }
    }
}

impl RollbackControllerConfig {
    fn effective_hysteresis_windows(&self) -> usize {
        self.regression_hysteresis_windows.max(1)
    }
}

/// Controller action selected for one safety window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackAction {
    /// Controller is disabled and no candidate state remains active.
    Disabled,
    /// No active candidate exists.
    Noop,
    /// Active candidate remains under evaluation.
    ContinueCandidate,
    /// Candidate passed safety checks and can become the last safe value.
    AcceptCandidate,
    /// Candidate regressed and must be restored to the last safe value.
    Rollback,
    /// Controller is cooling down after rollback.
    Cooldown,
}

/// Machine-readable rollback controller decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollbackDecision {
    /// Mode used for this decision.
    pub mode: TuningMode,
    /// Selected controller action.
    pub action: RollbackAction,
    /// Active candidate knob, when present.
    pub knob_id: Option<TunableKnobId>,
    /// Candidate value under evaluation, when present.
    pub candidate_value: Option<f64>,
    /// Last safe value to restore on rollback.
    pub rollback_value: Option<f64>,
    /// Stable reason codes for operator logs and robot/cockpit surfaces.
    pub reason_codes: Vec<String>,
    /// Per-metric safety checks.
    pub checks: Vec<SafetyMetricCheck>,
    /// Consecutive regressed windows observed for this candidate.
    pub regression_windows: usize,
    /// Cooldown windows remaining after this decision.
    pub cooldown_remaining_windows: usize,
}

/// Fail-closed rollback controller for one active tuning candidate.
#[derive(Debug, Clone)]
pub struct RollbackController {
    config: RollbackControllerConfig,
    mode: TuningMode,
    active_candidate: Option<TuningCandidate>,
    last_safe_values: BTreeMap<TunableKnobId, f64>,
    regression_windows: usize,
    cooldown_remaining_windows: usize,
}

impl RollbackController {
    /// Create a rollback controller with empty candidate state.
    #[must_use]
    pub fn new(config: RollbackControllerConfig) -> Self {
        let mode = if config.enabled {
            TuningMode::Observe
        } else {
            TuningMode::Disabled
        };

        Self {
            config,
            mode,
            active_candidate: None,
            last_safe_values: BTreeMap::new(),
            regression_windows: 0,
            cooldown_remaining_windows: 0,
        }
    }

    /// Return the current rollback controller mode.
    #[must_use]
    pub const fn mode(&self) -> TuningMode {
        self.mode
    }

    /// Return the active candidate, when one is under safety evaluation.
    #[must_use]
    pub const fn active_candidate(&self) -> Option<&TuningCandidate> {
        self.active_candidate.as_ref()
    }

    /// Return the last known safe value for a knob.
    #[must_use]
    pub fn last_safe_value(&self, knob_id: TunableKnobId) -> Option<f64> {
        self.last_safe_values.get(&knob_id).copied()
    }

    /// Start evaluating a candidate unless the controller is disabled or cooling down.
    #[must_use]
    pub fn start_candidate(&mut self, candidate: TuningCandidate) -> bool {
        if !self.config.enabled || self.mode == TuningMode::Disabled {
            self.disable();
            return false;
        }

        if self.cooldown_remaining_windows > 0 || self.mode == TuningMode::Cooldown {
            return false;
        }

        self.last_safe_values
            .entry(candidate.knob_id)
            .or_insert(candidate.old_value);
        self.mode = if candidate.live_mutation_allowed {
            TuningMode::SteadyState
        } else if candidate.would_apply {
            TuningMode::Exploration
        } else {
            TuningMode::Observe
        };
        self.regression_windows = 0;
        self.active_candidate = Some(candidate);
        true
    }

    /// Disable the controller and clear all partial candidate state.
    pub fn disable(&mut self) {
        self.config.enabled = false;
        self.mode = TuningMode::Disabled;
        self.active_candidate = None;
        self.regression_windows = 0;
        self.cooldown_remaining_windows = 0;
    }

    /// Evaluate one safety window and choose continue, accept, rollback, or cooldown.
    pub fn evaluate(&mut self, telemetry: &SafetyTelemetryWindow) -> RollbackDecision {
        if !self.config.enabled || self.mode == TuningMode::Disabled {
            self.disable();
            return self.decision_for(
                TuningMode::Disabled,
                RollbackAction::Disabled,
                None,
                None,
                vec!["auto_tune.disabled".to_string()],
                Vec::new(),
            );
        }

        if self.mode == TuningMode::Cooldown {
            if self.cooldown_remaining_windows > 0 {
                self.cooldown_remaining_windows -= 1;
                let decision = self.decision_for(
                    TuningMode::Cooldown,
                    RollbackAction::Cooldown,
                    None,
                    None,
                    vec!["auto_tune.cooldown.active".to_string()],
                    Vec::new(),
                );
                if self.cooldown_remaining_windows == 0 {
                    self.mode = TuningMode::Observe;
                }
                return decision;
            }
            self.mode = TuningMode::Observe;
        }

        let Some(candidate) = self.active_candidate.clone() else {
            return self.decision_for(
                self.mode,
                RollbackAction::Noop,
                None,
                None,
                vec!["auto_tune.safety.no_active_candidate".to_string()],
                Vec::new(),
            );
        };

        if !telemetry.warmup_complete {
            return self.decision_for(
                self.mode,
                RollbackAction::ContinueCandidate,
                Some(&candidate),
                None,
                vec!["auto_tune.safety.warmup_incomplete".to_string()],
                Vec::new(),
            );
        }

        if !telemetry.confidence.is_finite()
            || !telemetry.minimum_confidence.is_finite()
            || telemetry.confidence < telemetry.minimum_confidence
        {
            return self.rollback_candidate(
                &candidate,
                vec!["auto_tune.rollback.insufficient_confidence".to_string()],
                Vec::new(),
            );
        }

        let checks = telemetry
            .samples
            .iter()
            .map(SafetyMetricCheck::from_sample)
            .collect::<Vec<_>>();

        if checks.is_empty() {
            return self.rollback_candidate(
                &candidate,
                vec!["auto_tune.rollback.missing_telemetry".to_string()],
                checks,
            );
        }

        let mut regression_codes = checks
            .iter()
            .filter(|check| check.should_rollback())
            .map(|check| check.reason_code.clone())
            .collect::<Vec<_>>();

        if !regression_codes.is_empty() {
            self.regression_windows += 1;
            if self.regression_windows < self.config.effective_hysteresis_windows() {
                regression_codes.push("auto_tune.safety.regression_hysteresis".to_string());
                return self.decision_for(
                    self.mode,
                    RollbackAction::ContinueCandidate,
                    Some(&candidate),
                    None,
                    regression_codes,
                    checks,
                );
            }

            regression_codes.push("auto_tune.rollback.metric_regression".to_string());
            return self.rollback_candidate(&candidate, regression_codes, checks);
        }

        self.accept_candidate(&candidate, checks)
    }

    fn accept_candidate(
        &mut self,
        candidate: &TuningCandidate,
        checks: Vec<SafetyMetricCheck>,
    ) -> RollbackDecision {
        self.regression_windows = 0;
        let safe_value = if candidate.would_apply {
            candidate.candidate_value
        } else {
            candidate.old_value
        };
        self.last_safe_values.insert(candidate.knob_id, safe_value);
        self.active_candidate = None;
        self.mode = if candidate.would_apply {
            TuningMode::SteadyState
        } else {
            TuningMode::Observe
        };
        self.decision_for(
            self.mode,
            RollbackAction::AcceptCandidate,
            Some(candidate),
            None,
            vec!["auto_tune.safety.accepted".to_string()],
            checks,
        )
    }

    fn rollback_candidate(
        &mut self,
        candidate: &TuningCandidate,
        reason_codes: Vec<String>,
        checks: Vec<SafetyMetricCheck>,
    ) -> RollbackDecision {
        let rollback_value = self
            .last_safe_values
            .get(&candidate.knob_id)
            .copied()
            .unwrap_or(candidate.old_value);
        self.last_safe_values
            .insert(candidate.knob_id, rollback_value);
        self.mode = TuningMode::Rollback;
        self.cooldown_remaining_windows = self.config.cooldown_windows;

        let decision = self.decision_for(
            TuningMode::Rollback,
            RollbackAction::Rollback,
            Some(candidate),
            Some(rollback_value),
            reason_codes,
            checks,
        );

        self.active_candidate = None;
        self.regression_windows = 0;
        self.mode = if self.cooldown_remaining_windows > 0 {
            TuningMode::Cooldown
        } else {
            TuningMode::Observe
        };

        decision
    }

    fn decision_for(
        &self,
        mode: TuningMode,
        action: RollbackAction,
        candidate: Option<&TuningCandidate>,
        rollback_value: Option<f64>,
        reason_codes: Vec<String>,
        checks: Vec<SafetyMetricCheck>,
    ) -> RollbackDecision {
        RollbackDecision {
            mode,
            action,
            knob_id: candidate.map(|candidate| candidate.knob_id),
            candidate_value: candidate.map(|candidate| candidate.candidate_value),
            rollback_value,
            reason_codes,
            checks,
            regression_windows: self.regression_windows,
            cooldown_remaining_windows: self.cooldown_remaining_windows,
        }
    }
}

fn metric_reason_code(metric: SafetyMetricKind, suffix: &str) -> String {
    format!("auto_tune.safety.{}.{}", metric.as_str(), suffix)
}

// =============================================================================
// Decision records and bounded operator log
// =============================================================================

/// Schema version for auto-tune decision records surfaced to operators.
pub const AUTO_TUNE_DECISION_RECORD_SCHEMA_VERSION: u32 = 1;

/// Default number of recent auto-tune decisions retained for operator surfaces.
pub const DEFAULT_TUNING_DECISION_LOG_CAPACITY: usize = 64;

const MAX_TUNING_DECISION_REASON_CODES: usize = 8;
const MAX_TUNING_DECISION_SAFETY_CHECKS: usize = 8;

/// Operator-visible decision family for the candidate and rollback controllers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuningDecisionKind {
    /// A bounded candidate was emitted and entered observation/exploration.
    CandidateStarted,
    /// A candidate passed safety checks.
    CandidateAccepted,
    /// A candidate failed safety checks before rollback action is reported.
    CandidateRejected,
    /// A rollback value should be restored.
    Rollback,
    /// The controller is intentionally cooling down.
    Cooldown,
    /// Auto-tuning is disabled.
    Disabled,
    /// Exploration was skipped before a candidate was produced.
    ExplorationSkipped,
    /// Controller is observing or waiting for more evidence.
    Observing,
    /// Controller is in steady state after an accepted candidate.
    SteadyState,
}

impl TuningDecisionKind {
    /// Stable label for compact cockpit rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CandidateStarted => "candidate_started",
            Self::CandidateAccepted => "candidate_accepted",
            Self::CandidateRejected => "candidate_rejected",
            Self::Rollback => "rollback",
            Self::Cooldown => "cooldown",
            Self::Disabled => "disabled",
            Self::ExplorationSkipped => "exploration_skipped",
            Self::Observing => "observing",
            Self::SteadyState => "steady_state",
        }
    }
}

/// Confidence classification used by compact and JSON operator telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuningConfidenceState {
    /// Confidence telemetry was absent or invalid.
    Missing,
    /// Confidence telemetry existed but was below the configured floor.
    Insufficient,
    /// Confidence telemetry met the configured floor.
    Acceptable,
}

impl TuningConfidenceState {
    fn from_confidence(confidence: f64, minimum_confidence: f64) -> Self {
        if !confidence.is_finite() || !minimum_confidence.is_finite() {
            Self::Missing
        } else if confidence < minimum_confidence {
            Self::Insufficient
        } else {
            Self::Acceptable
        }
    }

    /// Stable label for compact cockpit rows.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Insufficient => "insufficient",
            Self::Acceptable => "acceptable",
        }
    }
}

/// Bounded summary of the telemetry window that produced a decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningMetricWindowSummary {
    /// Whether the warmup portion of the window was complete.
    pub warmup_complete: bool,
    /// Number of measurements represented, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement_count: Option<usize>,
    /// Required measurement floor, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_measurements: Option<usize>,
    /// Confidence score, when finite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    /// Required confidence floor, when finite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_confidence: Option<f64>,
    /// Stable confidence classification.
    pub confidence_state: TuningConfidenceState,
}

impl TuningMetricWindowSummary {
    /// Summarize a candidate-generation telemetry window.
    #[must_use]
    pub fn from_candidate_window(window: &CandidateTelemetryWindow) -> Self {
        Self {
            warmup_complete: window.warmup_complete,
            measurement_count: Some(window.measurement_count),
            minimum_measurements: Some(window.minimum_measurements),
            confidence: finite_option(window.confidence),
            minimum_confidence: finite_option(window.minimum_confidence),
            confidence_state: TuningConfidenceState::from_confidence(
                window.confidence,
                window.minimum_confidence,
            ),
        }
    }

    /// Summarize a rollback safety telemetry window.
    #[must_use]
    pub fn from_safety_window(window: &SafetyTelemetryWindow) -> Self {
        Self {
            warmup_complete: window.warmup_complete,
            measurement_count: Some(window.samples.len()),
            minimum_measurements: None,
            confidence: finite_option(window.confidence),
            minimum_confidence: finite_option(window.minimum_confidence),
            confidence_state: TuningConfidenceState::from_confidence(
                window.confidence,
                window.minimum_confidence,
            ),
        }
    }
}

/// Bounded per-metric safety check exposed in a decision record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningSafetyCheckSummary {
    /// Metric family checked.
    pub metric: SafetyMetricKind,
    /// Per-metric verdict.
    pub verdict: SafetyMetricVerdict,
    /// Stable reason code.
    pub reason_code: String,
    /// Last safe baseline, when finite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<f64>,
    /// Candidate-window observation, when finite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<f64>,
    /// Computed monotonic safety limit, when finite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    /// Whether this check can fail the candidate closed.
    pub required: bool,
}

impl From<&SafetyMetricCheck> for TuningSafetyCheckSummary {
    fn from(check: &SafetyMetricCheck) -> Self {
        Self {
            metric: check.metric,
            verdict: check.verdict,
            reason_code: check.reason_code.clone(),
            baseline: check.baseline.and_then(finite_option),
            observed: check.observed.and_then(finite_option),
            limit: check.limit.and_then(finite_option),
            required: check.required,
        }
    }
}

/// Machine-readable auto-tune decision record for logs, robot APIs, and cockpit rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningDecisionRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Decision timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
    /// Operator profile or rollout scope that produced the decision.
    pub profile: String,
    /// Correlation id linking candidate and rollback records.
    pub correlation_id: String,
    /// Decision family.
    pub kind: TuningDecisionKind,
    /// Controller mode at decision time.
    pub mode: TuningMode,
    /// Tunable knob, when a decision targets one knob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knob_id: Option<TunableKnobId>,
    /// Stable knob label for compact renderers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knob_name: Option<String>,
    /// Prior knob value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<f64>,
    /// Candidate or accepted knob value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_value: Option<f64>,
    /// Value to restore on rollback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_value: Option<f64>,
    /// Registry gate attached to this knob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate: Option<KnobGate>,
    /// Whether the controller intended to apply this candidate.
    pub would_apply: bool,
    /// Whether live mutation was allowed for this decision.
    pub live_mutation_allowed: bool,
    /// Stable reason codes, bounded for high-scale runs.
    pub reason_codes: Vec<String>,
    /// Bounded telemetry-window summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric_window: Option<TuningMetricWindowSummary>,
    /// Bounded safety-check summaries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safety_checks: Vec<TuningSafetyCheckSummary>,
    /// Active exploration count at decision time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_explorations: Option<usize>,
    /// Configured active exploration limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_explorations: Option<usize>,
}

impl TuningDecisionRecord {
    /// Build a record from candidate generation or a skipped exploration decision.
    #[must_use]
    pub fn from_candidate_decision(
        timestamp_ms: u64,
        profile: impl Into<String>,
        correlation_id: impl Into<String>,
        decision: &CandidateDecision,
        telemetry: &CandidateTelemetryWindow,
    ) -> Self {
        match &decision.candidate {
            Some(candidate) => Self::from_candidate(
                timestamp_ms,
                profile,
                correlation_id,
                TuningDecisionKind::CandidateStarted,
                decision.mode,
                candidate,
                vec![candidate.reason_code.clone()],
                Some(TuningMetricWindowSummary::from_candidate_window(telemetry)),
                Some(decision.active_explorations),
                Some(decision.max_concurrent_explorations),
            ),
            None => Self {
                schema_version: AUTO_TUNE_DECISION_RECORD_SCHEMA_VERSION,
                timestamp_ms,
                profile: profile.into(),
                correlation_id: correlation_id.into(),
                kind: TuningDecisionKind::ExplorationSkipped,
                mode: decision.mode,
                knob_id: None,
                knob_name: None,
                old_value: None,
                new_value: None,
                rollback_value: None,
                gate: None,
                would_apply: decision.mode.would_apply_candidate(),
                live_mutation_allowed: decision.mode.may_mutate_live_knobs(),
                reason_codes: bounded_reason_codes(
                    decision
                        .skip_reasons
                        .iter()
                        .map(CandidateSkipReason::reason_code),
                ),
                metric_window: Some(TuningMetricWindowSummary::from_candidate_window(telemetry)),
                safety_checks: Vec::new(),
                active_explorations: Some(decision.active_explorations),
                max_concurrent_explorations: Some(decision.max_concurrent_explorations),
            },
        }
    }

    /// Build one record from a rollback-controller decision.
    #[must_use]
    pub fn from_rollback_decision(
        timestamp_ms: u64,
        profile: impl Into<String>,
        correlation_id: impl Into<String>,
        decision: &RollbackDecision,
        telemetry: Option<&SafetyTelemetryWindow>,
    ) -> Self {
        let kind = match decision.action {
            RollbackAction::Disabled => TuningDecisionKind::Disabled,
            RollbackAction::Noop | RollbackAction::ContinueCandidate => {
                TuningDecisionKind::Observing
            }
            RollbackAction::AcceptCandidate => TuningDecisionKind::CandidateAccepted,
            RollbackAction::Rollback => TuningDecisionKind::Rollback,
            RollbackAction::Cooldown => TuningDecisionKind::Cooldown,
        };

        Self {
            schema_version: AUTO_TUNE_DECISION_RECORD_SCHEMA_VERSION,
            timestamp_ms,
            profile: profile.into(),
            correlation_id: correlation_id.into(),
            kind,
            mode: decision.mode,
            knob_id: decision.knob_id,
            knob_name: decision.knob_id.map(|knob_id| knob_id.as_str().to_string()),
            old_value: None,
            new_value: decision.candidate_value.and_then(finite_option),
            rollback_value: decision.rollback_value.and_then(finite_option),
            gate: None,
            would_apply: decision.mode.would_apply_candidate(),
            live_mutation_allowed: decision.mode.may_mutate_live_knobs(),
            reason_codes: bounded_reason_codes(decision.reason_codes.iter().map(String::as_str)),
            metric_window: telemetry.map(TuningMetricWindowSummary::from_safety_window),
            safety_checks: decision
                .checks
                .iter()
                .take(MAX_TUNING_DECISION_SAFETY_CHECKS)
                .map(TuningSafetyCheckSummary::from)
                .collect(),
            active_explorations: None,
            max_concurrent_explorations: None,
        }
    }

    fn from_candidate(
        timestamp_ms: u64,
        profile: impl Into<String>,
        correlation_id: impl Into<String>,
        kind: TuningDecisionKind,
        mode: TuningMode,
        candidate: &TuningCandidate,
        reason_codes: Vec<String>,
        metric_window: Option<TuningMetricWindowSummary>,
        active_explorations: Option<usize>,
        max_concurrent_explorations: Option<usize>,
    ) -> Self {
        Self {
            schema_version: AUTO_TUNE_DECISION_RECORD_SCHEMA_VERSION,
            timestamp_ms,
            profile: profile.into(),
            correlation_id: correlation_id.into(),
            kind,
            mode,
            knob_id: Some(candidate.knob_id),
            knob_name: Some(candidate.knob_id.as_str().to_string()),
            old_value: finite_option(candidate.old_value),
            new_value: finite_option(candidate.candidate_value),
            rollback_value: None,
            gate: Some(candidate.gate),
            would_apply: candidate.would_apply,
            live_mutation_allowed: candidate.live_mutation_allowed,
            reason_codes: reason_codes
                .into_iter()
                .take(MAX_TUNING_DECISION_REASON_CODES)
                .collect(),
            metric_window,
            safety_checks: Vec::new(),
            active_explorations,
            max_concurrent_explorations,
        }
    }

    fn candidate_rejected_from_rollback(
        timestamp_ms: u64,
        profile: &str,
        correlation_id: &str,
        decision: &RollbackDecision,
        telemetry: Option<&SafetyTelemetryWindow>,
    ) -> Self {
        let mut record = Self::from_rollback_decision(
            timestamp_ms,
            profile,
            correlation_id,
            decision,
            telemetry,
        );
        record.kind = TuningDecisionKind::CandidateRejected;
        record
            .reason_codes
            .push("auto_tune.candidate.rejected".to_string());
        record
            .reason_codes
            .truncate(MAX_TUNING_DECISION_REASON_CODES);
        record
    }
}

/// Bounded ring buffer for recent auto-tune decision records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TuningDecisionLog {
    capacity: usize,
    records: VecDeque<TuningDecisionRecord>,
}

impl Default for TuningDecisionLog {
    fn default() -> Self {
        Self::new(DEFAULT_TUNING_DECISION_LOG_CAPACITY)
    }
}

impl TuningDecisionLog {
    /// Create a bounded decision log.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            records: VecDeque::new(),
        }
    }

    /// Return configured retention capacity.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Append one decision, evicting the oldest record when at capacity.
    pub fn push(&mut self, record: TuningDecisionRecord) {
        if self.records.len() >= self.capacity {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    /// Record candidate-generation or skipped-exploration output.
    pub fn record_candidate_decision(
        &mut self,
        timestamp_ms: u64,
        profile: &str,
        correlation_id: &str,
        decision: &CandidateDecision,
        telemetry: &CandidateTelemetryWindow,
    ) {
        self.push(TuningDecisionRecord::from_candidate_decision(
            timestamp_ms,
            profile,
            correlation_id,
            decision,
            telemetry,
        ));
    }

    /// Record rollback output. Rollback emits both rejection and rollback rows.
    pub fn record_rollback_decision(
        &mut self,
        timestamp_ms: u64,
        profile: &str,
        correlation_id: &str,
        decision: &RollbackDecision,
        telemetry: Option<&SafetyTelemetryWindow>,
    ) {
        if decision.action == RollbackAction::Rollback {
            self.push(TuningDecisionRecord::candidate_rejected_from_rollback(
                timestamp_ms,
                profile,
                correlation_id,
                decision,
                telemetry,
            ));
        }
        self.push(TuningDecisionRecord::from_rollback_decision(
            timestamp_ms,
            profile,
            correlation_id,
            decision,
            telemetry,
        ));
    }

    /// Recent decisions in oldest-to-newest order.
    #[must_use]
    pub fn recent(&self) -> Vec<TuningDecisionRecord> {
        self.records.iter().cloned().collect()
    }

    /// Number of retained decisions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

fn finite_option(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn bounded_reason_codes<'a>(codes: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    codes
        .into_iter()
        .take(MAX_TUNING_DECISION_REASON_CODES)
        .map(str::to_string)
        .collect()
}

// =============================================================================
// Deterministic replay proof reports
// =============================================================================

/// Schema version for deterministic auto-tune replay proof reports.
pub const AUTO_TUNE_REPLAY_PROOF_SCHEMA_VERSION: u32 = 1;

/// Evidence level attached to auto-tune proof artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoTuneEvidenceLevel {
    /// Local or small-worker proof of logic, schemas, and rollback behavior.
    LocalReduced,
    /// Remote `rch` proof reached Cargo/tests, but the worker is not target-class hardware.
    RemoteReduced,
    /// The proof ran on a worker satisfying the target high-scale predicate.
    TargetHardware,
    /// The required high-scale predicate or artifact was absent.
    SkippedNotProven,
}

impl AutoTuneEvidenceLevel {
    /// Stable evidence label for proof logs and Beads comments.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalReduced => "local_reduced",
            Self::RemoteReduced => "remote_reduced",
            Self::TargetHardware => "target_hardware",
            Self::SkippedNotProven => "skipped_not_proven",
        }
    }
}

/// One deterministic candidate-generation and safety-evaluation replay step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoTuneReplayStep {
    /// Candidate decision timestamp in epoch milliseconds.
    pub timestamp_ms: u64,
    /// Operator profile or replay scope.
    pub profile: String,
    /// Correlation id shared by candidate and rollback rows.
    pub correlation_id: String,
    /// Candidate engine configuration for this step.
    pub engine_config: CandidateEvaluationConfig,
    /// Active exploration count reported to the candidate engine.
    pub active_explorations: usize,
    /// Telemetry window used to produce or skip a candidate.
    pub candidate_telemetry: CandidateTelemetryWindow,
    /// Optional requested knobs. Empty means registry-order candidate selection.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_knobs: Vec<String>,
    /// Knobs pinned for this replay step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_knobs: Vec<TunableKnobId>,
    /// Rollback controller configuration for this replay step.
    pub rollback_config: RollbackControllerConfig,
    /// Safety windows observed after a candidate starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub safety_windows: Vec<SafetyTelemetryWindow>,
}

/// Deterministic fixed-trace replay input for the auto-tune controllers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoTuneReplayTrace {
    /// Trace schema version.
    pub schema_version: u32,
    /// Stable trace identifier.
    pub trace_id: String,
    /// Evidence level for the replay itself.
    pub evidence_level: AutoTuneEvidenceLevel,
    /// Whether this trace was collected on 64-core / 256 GiB target-class hardware.
    pub target_hardware_predicate_met: bool,
    /// Current knob values before the replay begins.
    pub initial_values: BTreeMap<TunableKnobId, f64>,
    /// Ordered candidate/safety replay steps.
    pub steps: Vec<AutoTuneReplayStep>,
    /// Retained proof artifacts backing this trace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
}

/// Aggregate counters and safety assertions for a replay proof report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoTuneReplaySummary {
    /// Number of replay steps processed.
    pub steps: usize,
    /// Number of candidate-start decisions.
    pub candidates_started: usize,
    /// Number of accepted candidates.
    pub candidates_accepted: usize,
    /// Number of rejected candidates.
    pub candidates_rejected: usize,
    /// Number of rollback actions.
    pub rollbacks: usize,
    /// Number of skipped explorations before a candidate started.
    pub explorations_skipped: usize,
    /// Skips caused specifically by missing or stale candidate telemetry.
    pub missing_or_stale_telemetry_noops: usize,
    /// Controller windows that continued observation without accepting or rolling back.
    pub observing_windows: usize,
    /// Whether every accepted candidate preserved or improved required safety metrics.
    pub accepted_candidates_preserved_or_improved: bool,
    /// Whether every rejected candidate produced a rollback action.
    pub regressed_candidates_rolled_back: bool,
}

impl AutoTuneReplaySummary {
    fn new(steps: usize) -> Self {
        Self {
            steps,
            candidates_started: 0,
            candidates_accepted: 0,
            candidates_rejected: 0,
            rollbacks: 0,
            explorations_skipped: 0,
            missing_or_stale_telemetry_noops: 0,
            observing_windows: 0,
            accepted_candidates_preserved_or_improved: true,
            regressed_candidates_rolled_back: true,
        }
    }
}

/// Deterministic fixed-trace replay report for auto-tune proof artifacts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoTuneReplayProofReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Stable trace identifier.
    pub trace_id: String,
    /// Evidence level for the replay itself.
    pub evidence_level: AutoTuneEvidenceLevel,
    /// High-scale claim status after applying the hardware predicate.
    pub high_scale_evidence_level: AutoTuneEvidenceLevel,
    /// Whether this report may support a high-scale benefit claim.
    pub high_scale_claim_allowed: bool,
    /// Stable reason code for the high-scale evidence decision.
    pub high_scale_reason_code: String,
    /// Current knob values before replay.
    pub before_values: BTreeMap<TunableKnobId, f64>,
    /// Simulated knob values after accepted candidates and rollbacks.
    pub after_values: BTreeMap<TunableKnobId, f64>,
    /// Aggregate replay counters and assertions.
    pub summary: AutoTuneReplaySummary,
    /// Structured decision rows emitted by the replay.
    pub decisions: Vec<TuningDecisionRecord>,
    /// Retained proof artifacts backing this report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
}

/// Run a deterministic auto-tune trace through the candidate and rollback controllers.
#[must_use]
pub fn run_auto_tune_replay_trace(trace: &AutoTuneReplayTrace) -> AutoTuneReplayProofReport {
    let mut current_values = trace.initial_values.clone();
    let mut summary = AutoTuneReplaySummary::new(trace.steps.len());
    let mut log = TuningDecisionLog::new(replay_decision_capacity(trace));

    for step in &trace.steps {
        let mut engine = BoundedCandidateEngine::new(step.engine_config.clone());
        engine.set_active_explorations(step.active_explorations);
        let requested_knobs = step
            .requested_knobs
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let candidate_decision = if requested_knobs.is_empty() {
            engine.evaluate(
                &current_values,
                &step.candidate_telemetry,
                &step.pinned_knobs,
            )
        } else {
            engine.evaluate_requested(
                &requested_knobs,
                &current_values,
                &step.candidate_telemetry,
                &step.pinned_knobs,
            )
        };

        log.record_candidate_decision(
            step.timestamp_ms,
            &step.profile,
            &step.correlation_id,
            &candidate_decision,
            &step.candidate_telemetry,
        );

        let Some(candidate) = candidate_decision.candidate.clone() else {
            summary.explorations_skipped += 1;
            if candidate_decision
                .skip_reasons
                .iter()
                .any(is_missing_or_stale_candidate_telemetry)
            {
                summary.missing_or_stale_telemetry_noops += 1;
            }
            continue;
        };

        summary.candidates_started += 1;

        let mut rollback = RollbackController::new(step.rollback_config.clone());
        if !rollback.start_candidate(candidate.clone()) {
            summary.observing_windows += 1;
            continue;
        }

        for (window_index, safety_window) in step.safety_windows.iter().enumerate() {
            let decision = rollback.evaluate(safety_window);
            let safety_timestamp_ms = step
                .timestamp_ms
                .saturating_add(u64::try_from(window_index).unwrap_or(u64::MAX))
                .saturating_add(1);
            log.record_rollback_decision(
                safety_timestamp_ms,
                &step.profile,
                &step.correlation_id,
                &decision,
                Some(safety_window),
            );

            match decision.action {
                RollbackAction::AcceptCandidate => {
                    summary.candidates_accepted += 1;
                    if !required_checks_preserved_or_improved(&decision.checks) {
                        summary.accepted_candidates_preserved_or_improved = false;
                    }
                    if candidate.would_apply {
                        current_values.insert(candidate.knob_id, candidate.candidate_value);
                    }
                    break;
                }
                RollbackAction::Rollback => {
                    summary.candidates_rejected += 1;
                    summary.rollbacks += 1;
                    let rollback_value = decision.rollback_value.unwrap_or(candidate.old_value);
                    current_values.insert(candidate.knob_id, rollback_value);
                    break;
                }
                RollbackAction::ContinueCandidate => {
                    summary.observing_windows += 1;
                }
                RollbackAction::Disabled | RollbackAction::Noop | RollbackAction::Cooldown => {
                    summary.observing_windows += 1;
                    break;
                }
            }
        }
    }

    summary.regressed_candidates_rolled_back = summary.candidates_rejected == summary.rollbacks;
    let high_scale_evidence_level = high_scale_evidence_level(trace);

    AutoTuneReplayProofReport {
        schema_version: AUTO_TUNE_REPLAY_PROOF_SCHEMA_VERSION,
        trace_id: trace.trace_id.clone(),
        evidence_level: trace.evidence_level,
        high_scale_evidence_level,
        high_scale_claim_allowed: high_scale_evidence_level
            == AutoTuneEvidenceLevel::TargetHardware,
        high_scale_reason_code: high_scale_reason_code(high_scale_evidence_level).to_string(),
        before_values: trace.initial_values.clone(),
        after_values: current_values,
        summary,
        decisions: log.recent(),
        artifact_paths: trace.artifact_paths.clone(),
    }
}

fn replay_decision_capacity(trace: &AutoTuneReplayTrace) -> usize {
    trace
        .steps
        .iter()
        .map(|step| 1usize.saturating_add(step.safety_windows.len().saturating_mul(2)))
        .sum::<usize>()
        .max(1)
}

fn is_missing_or_stale_candidate_telemetry(reason: &CandidateSkipReason) -> bool {
    matches!(
        reason,
        CandidateSkipReason::MissingTelemetry | CandidateSkipReason::StaleTelemetry
    )
}

fn required_checks_preserved_or_improved(checks: &[SafetyMetricCheck]) -> bool {
    checks
        .iter()
        .filter(|check| check.required)
        .all(|check| check.verdict == SafetyMetricVerdict::Healthy)
}

fn high_scale_evidence_level(trace: &AutoTuneReplayTrace) -> AutoTuneEvidenceLevel {
    if trace.evidence_level == AutoTuneEvidenceLevel::TargetHardware
        && trace.target_hardware_predicate_met
    {
        AutoTuneEvidenceLevel::TargetHardware
    } else {
        AutoTuneEvidenceLevel::SkippedNotProven
    }
}

fn high_scale_reason_code(level: AutoTuneEvidenceLevel) -> &'static str {
    match level {
        AutoTuneEvidenceLevel::TargetHardware => "auto_tune.proof.high_scale.target_hardware",
        AutoTuneEvidenceLevel::LocalReduced
        | AutoTuneEvidenceLevel::RemoteReduced
        | AutoTuneEvidenceLevel::SkippedNotProven => {
            "auto_tune.proof.high_scale.skipped_not_proven"
        }
    }
}

// =============================================================================
// Manual overrides
// =============================================================================

/// Which parameters are pinned (manually overridden).
///
/// When a parameter is pinned, the auto-tuner will not modify it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinnedParams {
    pub poll_interval_ms: bool,
    pub scrollback_lines: bool,
    pub snapshot_interval_secs: bool,
    pub pool_size: bool,
    pub backpressure_threshold: bool,
}

// =============================================================================
// Tuning config
// =============================================================================

/// Configuration for the auto-tuner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoTuneConfig {
    /// Whether auto-tuning may mutate tunable parameters.
    ///
    /// Defaults to `false`; operators must opt in before the legacy
    /// proportional tuner changes live parameters.
    pub enabled: bool,
    /// Tick interval (seconds).
    pub tick_interval_secs: u64,
    /// Tuning targets.
    pub targets: TuningTargets,
    /// Maximum fractional change per tick (e.g. 0.1 = 10%).
    pub max_change_per_tick: f64,
    /// Number of sustained ticks of signal before making a change.
    pub hysteresis_ticks: usize,
    /// Maximum metrics history to keep.
    pub history_limit: usize,
}

impl Default for AutoTuneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tick_interval_secs: 30,
            targets: TuningTargets::default(),
            max_change_per_tick: 0.1,
            hysteresis_ticks: 3,
            history_limit: 100,
        }
    }
}

// =============================================================================
// Adjustment record
// =============================================================================

/// Records a single parameter adjustment with reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Adjustment {
    /// Which parameter was adjusted.
    pub param: String,
    /// Old value.
    pub old_value: f64,
    /// New value.
    pub new_value: f64,
    /// Pressure ratio that triggered the adjustment.
    pub pressure: f64,
    /// Human-readable reason.
    pub reason: String,
}

// =============================================================================
// Hysteresis state
// =============================================================================

/// Tracks sustained signal direction for hysteresis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressureDirection {
    None,
    Increase,
    Decrease,
}

#[derive(Debug, Clone)]
struct HysteresisState {
    memory_direction: PressureDirection,
    memory_ticks: usize,
    latency_direction: PressureDirection,
    latency_ticks: usize,
    cpu_direction: PressureDirection,
    cpu_ticks: usize,
}

impl HysteresisState {
    fn new() -> Self {
        Self {
            memory_direction: PressureDirection::None,
            memory_ticks: 0,
            latency_direction: PressureDirection::None,
            latency_ticks: 0,
            cpu_direction: PressureDirection::None,
            cpu_ticks: 0,
        }
    }

    /// Update a pressure direction counter. Returns true if sustained threshold is met.
    fn update(
        direction: &mut PressureDirection,
        ticks: &mut usize,
        new_dir: PressureDirection,
        threshold: usize,
    ) -> bool {
        if *direction == new_dir {
            *ticks += 1;
        } else {
            *direction = new_dir;
            *ticks = 1;
        }
        *ticks >= threshold
    }
}

// =============================================================================
// AutoTuner
// =============================================================================

/// Proportional control loop for adaptive parameter tuning.
///
/// Call `tick()` with each new set of system metrics. The tuner adjusts
/// parameters gradually (max 10% per tick by default) with hysteresis
/// to prevent oscillation.
#[derive(Debug)]
pub struct AutoTuner {
    /// Current tuned parameters.
    params: TunableParams,
    /// Configuration.
    config: AutoTuneConfig,
    /// Manual overrides.
    pinned: PinnedParams,
    /// Metrics history (bounded).
    history: VecDeque<TunerMetrics>,
    /// Hysteresis tracking.
    hysteresis: HysteresisState,
    /// Log of adjustments made.
    adjustments: Vec<Adjustment>,
    /// Total ticks processed.
    tick_count: u64,
}

impl AutoTuner {
    /// Create a new auto-tuner with default parameters.
    #[must_use]
    pub fn new(config: AutoTuneConfig) -> Self {
        Self {
            params: TunableParams::default(),
            config,
            pinned: PinnedParams::default(),
            history: VecDeque::new(),
            hysteresis: HysteresisState::new(),
            adjustments: Vec::new(),
            tick_count: 0,
        }
    }

    /// Create a new auto-tuner with specified initial parameters.
    #[must_use]
    pub fn with_params(config: AutoTuneConfig, params: TunableParams) -> Self {
        Self {
            params,
            config,
            pinned: PinnedParams::default(),
            history: VecDeque::new(),
            hysteresis: HysteresisState::new(),
            adjustments: Vec::new(),
            tick_count: 0,
        }
    }

    /// Pin a parameter so the auto-tuner will not modify it.
    pub fn set_pinned(&mut self, pinned: PinnedParams) {
        self.pinned = pinned;
    }

    /// Get the pinned parameter state.
    #[must_use]
    pub fn pinned(&self) -> &PinnedParams {
        &self.pinned
    }

    /// Get the current tuned parameters.
    #[must_use]
    pub fn params(&self) -> &TunableParams {
        &self.params
    }

    /// Get the total number of ticks processed.
    #[must_use]
    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }

    /// Get the adjustments log.
    #[must_use]
    pub fn adjustments(&self) -> &[Adjustment] {
        &self.adjustments
    }

    /// Clear the adjustments log.
    pub fn clear_adjustments(&mut self) {
        self.adjustments.clear();
    }

    /// Process one tick of system metrics and return the adjusted parameters.
    pub fn tick(&mut self, metrics: &TunerMetrics) -> TunableParams {
        self.tick_count += 1;

        // Add to history (bounded)
        self.history.push_back(metrics.clone());
        if self.history.len() > self.config.history_limit {
            self.history.pop_front();
        }

        if !self.config.enabled {
            return self.params.clone();
        }

        let threshold = self.config.hysteresis_ticks;

        // --- Memory pressure ---
        let memory_pressure = metrics.rss_fraction / self.config.targets.target_rss_fraction;
        let memory_dir = if memory_pressure > 1.05 {
            PressureDirection::Increase
        } else if memory_pressure < 0.95 {
            PressureDirection::Decrease
        } else {
            PressureDirection::None
        };

        let memory_sustained = HysteresisState::update(
            &mut self.hysteresis.memory_direction,
            &mut self.hysteresis.memory_ticks,
            memory_dir,
            threshold,
        );

        if memory_sustained && memory_dir != PressureDirection::None {
            if memory_dir == PressureDirection::Increase {
                // High memory → reduce scrollback, increase snapshot interval
                if !self.pinned.scrollback_lines {
                    let old = self.params.scrollback_lines;
                    self.params.scrollback_lines =
                        self.apply_gradual_change(old, old / memory_pressure);
                    if (old - self.params.scrollback_lines).abs() > 0.01 {
                        self.adjustments.push(Adjustment {
                            param: "scrollback_lines".to_string(),
                            old_value: old,
                            new_value: self.params.scrollback_lines,
                            pressure: memory_pressure,
                            reason: "memory pressure".to_string(),
                        });
                    }
                }
                if !self.pinned.snapshot_interval_secs {
                    let old = self.params.snapshot_interval_secs;
                    self.params.snapshot_interval_secs =
                        self.apply_gradual_change(old, old * memory_pressure);
                    if (old - self.params.snapshot_interval_secs).abs() > 0.01 {
                        self.adjustments.push(Adjustment {
                            param: "snapshot_interval_secs".to_string(),
                            old_value: old,
                            new_value: self.params.snapshot_interval_secs,
                            pressure: memory_pressure,
                            reason: "memory pressure".to_string(),
                        });
                    }
                }
            } else {
                // Low memory usage → restore scrollback, reduce snapshot interval
                if !self.pinned.scrollback_lines {
                    let old = self.params.scrollback_lines;
                    let target = (old / memory_pressure).min(SCROLLBACK_LINES_RANGE.max);
                    self.params.scrollback_lines = self.apply_gradual_change(old, target);
                }
                if !self.pinned.snapshot_interval_secs {
                    let old = self.params.snapshot_interval_secs;
                    let target = (old * memory_pressure).max(SNAPSHOT_INTERVAL_RANGE.min);
                    self.params.snapshot_interval_secs = self.apply_gradual_change(old, target);
                }
            }
        }

        // --- Latency pressure ---
        let latency_pressure = metrics.mux_latency_ms / self.config.targets.target_latency_ms;
        let latency_dir = if latency_pressure > 1.05 {
            PressureDirection::Increase
        } else if latency_pressure < 0.95 {
            PressureDirection::Decrease
        } else {
            PressureDirection::None
        };

        let latency_sustained = HysteresisState::update(
            &mut self.hysteresis.latency_direction,
            &mut self.hysteresis.latency_ticks,
            latency_dir,
            threshold,
        );

        if latency_sustained && latency_dir != PressureDirection::None {
            if latency_dir == PressureDirection::Increase {
                // High latency → increase poll interval (poll less often)
                if !self.pinned.poll_interval_ms {
                    let old = self.params.poll_interval_ms;
                    self.params.poll_interval_ms =
                        self.apply_gradual_change(old, old * latency_pressure);
                    if (old - self.params.poll_interval_ms).abs() > 0.01 {
                        self.adjustments.push(Adjustment {
                            param: "poll_interval_ms".to_string(),
                            old_value: old,
                            new_value: self.params.poll_interval_ms,
                            pressure: latency_pressure,
                            reason: "latency pressure".to_string(),
                        });
                    }
                }
            } else {
                // Low latency → decrease poll interval (poll more often)
                if !self.pinned.poll_interval_ms {
                    let old = self.params.poll_interval_ms;
                    let target = (old * latency_pressure).max(POLL_INTERVAL_RANGE.min);
                    self.params.poll_interval_ms = self.apply_gradual_change(old, target);
                }
            }
        }

        // --- CPU pressure ---
        let cpu_pressure = metrics.cpu_fraction / self.config.targets.target_cpu_fraction;
        let cpu_dir = if cpu_pressure > 1.05 {
            PressureDirection::Increase
        } else if cpu_pressure < 0.95 {
            PressureDirection::Decrease
        } else {
            PressureDirection::None
        };

        let cpu_sustained = HysteresisState::update(
            &mut self.hysteresis.cpu_direction,
            &mut self.hysteresis.cpu_ticks,
            cpu_dir,
            threshold,
        );

        if cpu_sustained && cpu_dir != PressureDirection::None {
            if cpu_dir == PressureDirection::Increase {
                // High CPU → increase poll interval, reduce pool size
                if !self.pinned.poll_interval_ms {
                    let old = self.params.poll_interval_ms;
                    let target = old * cpu_pressure;
                    self.params.poll_interval_ms = self.apply_gradual_change(old, target);
                }
                if !self.pinned.pool_size {
                    let old = self.params.pool_size;
                    self.params.pool_size = self.apply_gradual_change(old, old / cpu_pressure);
                    if (old - self.params.pool_size).abs() > 0.01 {
                        self.adjustments.push(Adjustment {
                            param: "pool_size".to_string(),
                            old_value: old,
                            new_value: self.params.pool_size,
                            pressure: cpu_pressure,
                            reason: "CPU pressure".to_string(),
                        });
                    }
                }
            } else {
                // Low CPU → restore pool size
                if !self.pinned.pool_size {
                    let old = self.params.pool_size;
                    let target = (old / cpu_pressure).min(POOL_SIZE_RANGE.max);
                    self.params.pool_size = self.apply_gradual_change(old, target);
                }
            }
        }

        // Clamp all to safety ranges
        self.params.clamp_to_ranges();

        self.params.clone()
    }

    /// Apply a gradual change limited by max_change_per_tick.
    fn apply_gradual_change(&self, current: f64, target: f64) -> f64 {
        let max_delta = current * self.config.max_change_per_tick;
        let delta = target - current;
        if delta.abs() <= max_delta {
            target
        } else {
            delta.signum().mul_add(max_delta, current)
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn default_config() -> AutoTuneConfig {
        AutoTuneConfig {
            enabled: true,
            ..AutoTuneConfig::default()
        }
    }

    fn calm_metrics() -> TunerMetrics {
        // Values within the 0.95-1.05 deadband of the default targets
        // (target_rss_fraction=0.5, target_latency_ms=10.0, target_cpu_fraction=0.3)
        TunerMetrics {
            rss_fraction: 0.5,
            mux_latency_ms: 10.0,
            cpu_fraction: 0.3,
        }
    }

    fn high_memory_metrics() -> TunerMetrics {
        TunerMetrics {
            rss_fraction: 0.8,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.15,
        }
    }

    fn high_latency_metrics() -> TunerMetrics {
        TunerMetrics {
            rss_fraction: 0.3,
            mux_latency_ms: 25.0,
            cpu_fraction: 0.15,
        }
    }

    fn high_cpu_metrics() -> TunerMetrics {
        TunerMetrics {
            rss_fraction: 0.3,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.6,
        }
    }

    // ---- Basic tests ----

    #[test]
    fn default_params_within_ranges() {
        let params = TunableParams::default();
        assert!(params.poll_interval_ms >= POLL_INTERVAL_RANGE.min);
        assert!(params.poll_interval_ms <= POLL_INTERVAL_RANGE.max);
        assert!(params.scrollback_lines >= SCROLLBACK_LINES_RANGE.min);
        assert!(params.scrollback_lines <= SCROLLBACK_LINES_RANGE.max);
        assert!(params.snapshot_interval_secs >= SNAPSHOT_INTERVAL_RANGE.min);
        assert!(params.snapshot_interval_secs <= SNAPSHOT_INTERVAL_RANGE.max);
        assert!(params.pool_size >= POOL_SIZE_RANGE.min);
        assert!(params.pool_size <= POOL_SIZE_RANGE.max);
        assert!(params.backpressure_threshold >= BACKPRESSURE_THRESHOLD_RANGE.min);
        assert!(params.backpressure_threshold <= BACKPRESSURE_THRESHOLD_RANGE.max);
    }

    #[test]
    fn calm_metrics_no_change() {
        let mut tuner = AutoTuner::new(default_config());
        let initial = tuner.params().clone();

        // With calm metrics, nothing should change
        for _ in 0..10 {
            tuner.tick(&calm_metrics());
        }

        let p = tuner.params();
        assert!((p.poll_interval_ms - initial.poll_interval_ms).abs() < f64::EPSILON);
        assert!((p.scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON);
        assert!((p.snapshot_interval_secs - initial.snapshot_interval_secs).abs() < f64::EPSILON);
        assert!((p.pool_size - initial.pool_size).abs() < f64::EPSILON);
        assert!((p.backpressure_threshold - initial.backpressure_threshold).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_to_ranges_enforces_bounds() {
        let mut params = TunableParams {
            poll_interval_ms: 5.0,        // below min 100
            scrollback_lines: 50_000.0,   // above max 10000
            snapshot_interval_secs: 0.0,  // below min 60
            pool_size: 100.0,             // above max 16
            backpressure_threshold: -1.0, // below min 0.3
        };
        params.clamp_to_ranges();

        assert!(
            (params.poll_interval_ms - POLL_INTERVAL_RANGE.min).abs() < f64::EPSILON,
            "poll_interval_ms: {}",
            params.poll_interval_ms
        );
        assert!(
            (params.scrollback_lines - SCROLLBACK_LINES_RANGE.max).abs() < f64::EPSILON,
            "scrollback_lines: {}",
            params.scrollback_lines
        );
        assert!(
            (params.snapshot_interval_secs - SNAPSHOT_INTERVAL_RANGE.min).abs() < f64::EPSILON,
            "snapshot_interval_secs: {}",
            params.snapshot_interval_secs
        );
        assert!(
            (params.pool_size - POOL_SIZE_RANGE.max).abs() < f64::EPSILON,
            "pool_size: {}",
            params.pool_size
        );
        assert!(
            (params.backpressure_threshold - BACKPRESSURE_THRESHOLD_RANGE.min).abs() < f64::EPSILON,
            "backpressure_threshold: {}",
            params.backpressure_threshold
        );
    }

    #[test]
    fn integer_getters() {
        let params = TunableParams::default();
        assert_eq!(params.poll_interval_ms_u64(), 200);
        assert_eq!(params.scrollback_lines_usize(), 5000);
        assert_eq!(params.snapshot_interval_secs_u64(), 300);
        assert_eq!(params.pool_size_usize(), 4);
    }

    // ---- Bounded candidate engine tests ----

    fn fresh_observe_candidate_engine() -> BoundedCandidateEngine {
        BoundedCandidateEngine::new(CandidateEvaluationConfig::default())
    }

    fn pressure_window(direction: CandidateDirection) -> CandidateTelemetryWindow {
        CandidateTelemetryWindow {
            direction: Some(direction),
            ..CandidateTelemetryWindow::default()
        }
    }

    fn default_current_values(engine: &BoundedCandidateEngine) -> BTreeMap<TunableKnobId, f64> {
        engine
            .registry()
            .iter()
            .map(|(id, spec)| (*id, spec.default_value))
            .collect()
    }

    #[test]
    fn candidate_registry_rejects_unknown_knob() {
        let engine = fresh_observe_candidate_engine();
        assert_eq!(
            engine.validate_knob_id("not_a_real_knob"),
            Err(CandidateSkipReason::UnknownKnob)
        );
        assert_eq!(
            engine.validate_knob_id("runtime.output_coalesce_window_ms"),
            Ok(TunableKnobId::RuntimeOutputCoalesceWindowMs)
        );
    }

    #[test]
    fn candidate_engine_generates_single_registry_candidate() {
        let engine = fresh_observe_candidate_engine();
        let decision = engine.evaluate(
            &default_current_values(&engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );
        let candidate = decision.candidate.expect("registry candidate");

        assert_eq!(
            candidate.knob_id,
            TunableKnobId::RuntimeOutputCoalesceWindowMs
        );
        assert_eq!(candidate.owner, "runtime");
        assert_eq!(candidate.direction, CandidateDirection::Increase);
        assert!(!candidate.would_apply);
        assert!(!candidate.live_mutation_allowed);
        assert_eq!(candidate.candidate_value, 75.0);
        assert_eq!(
            candidate.reason_code,
            "auto_tune.candidate.runtime.output_coalesce_window_ms"
        );
        assert!(decision.skip_reasons.is_empty());
    }

    #[test]
    fn candidate_engine_respects_pinned_knobs() {
        let engine = fresh_observe_candidate_engine();
        let decision = engine.evaluate(
            &default_current_values(&engine),
            &pressure_window(CandidateDirection::Increase),
            &[TunableKnobId::RuntimeOutputCoalesceWindowMs],
        );
        let candidate = decision.candidate.expect("next registry candidate");

        assert_eq!(
            candidate.knob_id,
            TunableKnobId::RuntimeOutputCoalesceMaxDelayMs
        );
        assert_eq!(candidate.direction, CandidateDirection::Increase);
    }

    #[test]
    fn candidate_engine_skips_stale_telemetry() {
        let engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            telemetry_trust: TelemetryTrust::Stale,
            ..CandidateEvaluationConfig::default()
        });
        let decision = engine.evaluate(
            &default_current_values(&engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );

        assert!(decision.candidate.is_none());
        assert_eq!(
            decision.skip_reasons,
            vec![CandidateSkipReason::StaleTelemetry]
        );
    }

    #[test]
    fn candidate_engine_enforces_exploration_budget() {
        let mut engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            mode: TuningMode::Exploration,
            max_concurrent_explorations: 1,
            telemetry_trust: TelemetryTrust::Fresh,
        });
        engine.set_active_explorations(1);
        let decision = engine.evaluate(
            &default_current_values(&engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );

        assert!(decision.candidate.is_none());
        assert_eq!(
            decision.skip_reasons,
            vec![CandidateSkipReason::ExplorationBudgetExhausted]
        );
    }

    #[test]
    fn candidate_engine_marks_apply_scope_by_mode() {
        let engine = fresh_observe_candidate_engine();
        let observe = fresh_observe_candidate_engine().evaluate(
            &default_current_values(&engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );
        assert!(
            !observe
                .candidate
                .as_ref()
                .expect("observe candidate")
                .would_apply
        );
        assert!(
            !observe
                .candidate
                .as_ref()
                .expect("observe candidate")
                .live_mutation_allowed
        );

        let canary_engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            mode: TuningMode::Canary,
            ..CandidateEvaluationConfig::default()
        });
        let canary = canary_engine.evaluate(
            &default_current_values(&canary_engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );
        assert!(
            canary
                .candidate
                .as_ref()
                .expect("canary candidate")
                .would_apply
        );
        assert!(
            !canary
                .candidate
                .as_ref()
                .expect("canary candidate")
                .live_mutation_allowed
        );

        let steady_engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            mode: TuningMode::SteadyState,
            ..CandidateEvaluationConfig::default()
        });
        let steady = steady_engine.evaluate(
            &default_current_values(&steady_engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );
        assert!(
            steady
                .candidate
                .as_ref()
                .expect("steady candidate")
                .live_mutation_allowed
        );
    }

    #[test]
    fn candidate_engine_rejects_unsafe_multiple_knob_request() {
        let engine = fresh_observe_candidate_engine();
        let decision = engine.evaluate_requested(
            &[
                "runtime.output_coalesce_window_ms",
                "backpressure.warn_ratio",
            ],
            &default_current_values(&engine),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );

        assert!(decision.candidate.is_none());
        assert!(
            decision
                .skip_reasons
                .contains(&CandidateSkipReason::MultipleKnobsForbidden)
        );
        assert!(
            decision
                .skip_reasons
                .contains(&CandidateSkipReason::UnsafeCombination)
        );
    }

    #[test]
    fn candidate_engine_refuses_low_confidence_telemetry() {
        let engine = fresh_observe_candidate_engine();
        let decision = engine.evaluate(
            &default_current_values(&engine),
            &CandidateTelemetryWindow {
                confidence: 0.40,
                minimum_confidence: 0.80,
                direction: Some(CandidateDirection::Increase),
                ..CandidateTelemetryWindow::default()
            },
            &[],
        );

        assert!(decision.candidate.is_none());
        assert_eq!(
            decision.skip_reasons,
            vec![CandidateSkipReason::InsufficientConfidence]
        );
    }

    #[test]
    fn candidate_engine_requires_requested_current_value() {
        let engine = fresh_observe_candidate_engine();
        let decision = engine.evaluate_requested(
            &["runtime.output_coalesce_window_ms"],
            &BTreeMap::new(),
            &pressure_window(CandidateDirection::Increase),
            &[],
        );

        assert!(decision.candidate.is_none());
        assert_eq!(
            decision.skip_reasons,
            vec![CandidateSkipReason::MissingCurrentValue]
        );
    }

    #[test]
    fn registry_steps_stay_inside_hard_bounds() {
        let engine = fresh_observe_candidate_engine();

        for spec in engine.registry().values() {
            assert!(
                spec.range.contains(spec.default_value),
                "{}",
                spec.id.as_str()
            );
            assert_eq!(
                TunableKnobId::from_registry_id(spec.id.as_str()),
                Some(spec.id)
            );

            for direction in [CandidateDirection::Increase, CandidateDirection::Decrease] {
                let value = bounded_step(spec.default_value, spec.range, spec.step, direction)
                    .expect("bounded step");
                assert!(spec.range.contains(value), "{}", spec.id.as_str());
            }
        }
    }

    #[test]
    fn tuning_mode_contract_rejects_invalid_transition() {
        assert!(TuningMode::Disabled.can_transition_to(TuningMode::Observe));
        assert!(TuningMode::Observe.can_transition_to(TuningMode::Canary));
        assert!(TuningMode::Rollback.can_transition_to(TuningMode::Cooldown));
        assert!(TuningMode::Cooldown.can_transition_to(TuningMode::Observe));

        assert!(!TuningMode::Observe.can_transition_to(TuningMode::SteadyState));
        assert!(!TuningMode::Cooldown.can_transition_to(TuningMode::Exploration));
        assert!(!TuningMode::Rollback.can_transition_to(TuningMode::SteadyState));
    }

    // ---- Rollback controller tests ----

    fn candidate_for_rollback_controller() -> TuningCandidate {
        let engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            mode: TuningMode::Exploration,
            ..CandidateEvaluationConfig::default()
        });
        engine
            .evaluate(
                &default_current_values(&engine),
                &pressure_window(CandidateDirection::Increase),
                &[],
            )
            .candidate
            .expect("rollback controller candidate")
    }

    fn required_lower_sample(
        metric: SafetyMetricKind,
        baseline: Option<f64>,
        observed: Option<f64>,
    ) -> SafetyMetricSample {
        SafetyMetricSample {
            metric,
            baseline,
            observed,
            max_regression_fraction: 0.10,
            required: true,
            goal: SafetyMetricGoal::LowerOrEqual,
        }
    }

    fn optional_lower_sample(
        metric: SafetyMetricKind,
        baseline: Option<f64>,
        observed: Option<f64>,
    ) -> SafetyMetricSample {
        SafetyMetricSample {
            required: false,
            ..required_lower_sample(metric, baseline, observed)
        }
    }

    fn safety_window(samples: Vec<SafetyMetricSample>) -> SafetyTelemetryWindow {
        SafetyTelemetryWindow {
            samples,
            ..SafetyTelemetryWindow::default()
        }
    }

    fn default_replay_values() -> BTreeMap<TunableKnobId, f64> {
        let engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            mode: TuningMode::Exploration,
            ..CandidateEvaluationConfig::default()
        });
        default_current_values(&engine)
    }

    fn replay_step(
        correlation_id: &str,
        telemetry_trust: TelemetryTrust,
        safety_windows: Vec<SafetyTelemetryWindow>,
    ) -> AutoTuneReplayStep {
        AutoTuneReplayStep {
            timestamp_ms: 1_700_000_040_000,
            profile: "fixed-trace-replay".to_string(),
            correlation_id: correlation_id.to_string(),
            engine_config: CandidateEvaluationConfig {
                mode: TuningMode::Exploration,
                telemetry_trust,
                ..CandidateEvaluationConfig::default()
            },
            active_explorations: 0,
            candidate_telemetry: pressure_window(CandidateDirection::Increase),
            requested_knobs: Vec::new(),
            pinned_knobs: Vec::new(),
            rollback_config: RollbackControllerConfig {
                regression_hysteresis_windows: 1,
                cooldown_windows: 0,
                ..RollbackControllerConfig::default()
            },
            safety_windows,
        }
    }

    fn replay_trace(trace_id: &str, steps: Vec<AutoTuneReplayStep>) -> AutoTuneReplayTrace {
        AutoTuneReplayTrace {
            schema_version: AUTO_TUNE_REPLAY_PROOF_SCHEMA_VERSION,
            trace_id: trace_id.to_string(),
            evidence_level: AutoTuneEvidenceLevel::LocalReduced,
            target_hardware_predicate_met: false,
            initial_values: default_replay_values(),
            steps,
            artifact_paths: vec!["inline-fixed-trace".to_string()],
        }
    }

    #[test]
    fn rollback_controller_accepts_safe_window_and_updates_last_safe() {
        let mut controller = RollbackController::new(RollbackControllerConfig::default());
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate.clone()));
        let decision = controller.evaluate(&safety_window(vec![required_lower_sample(
            SafetyMetricKind::Latency,
            Some(100.0),
            Some(105.0),
        )]));

        assert_eq!(decision.action, RollbackAction::AcceptCandidate);
        assert_eq!(decision.mode, TuningMode::SteadyState);
        assert_eq!(decision.knob_id, Some(candidate.knob_id));
        assert_eq!(controller.active_candidate(), None);
        assert_eq!(
            controller.last_safe_value(candidate.knob_id),
            Some(candidate.candidate_value)
        );
        assert!(
            decision
                .reason_codes
                .contains(&"auto_tune.safety.accepted".to_string())
        );
    }

    #[test]
    fn rollback_controller_table_driven_monotonic_constraints() {
        let metrics = [
            SafetyMetricKind::Latency,
            SafetyMetricKind::QueueDepth,
            SafetyMetricKind::MemoryPressure,
            SafetyMetricKind::DroppedWork,
            SafetyMetricKind::ErrorRate,
            SafetyMetricKind::PolicyApprovalFailures,
        ];

        for metric in metrics {
            let mut controller = RollbackController::new(RollbackControllerConfig {
                regression_hysteresis_windows: 1,
                cooldown_windows: 0,
                ..RollbackControllerConfig::default()
            });
            let candidate = candidate_for_rollback_controller();

            assert!(controller.start_candidate(candidate.clone()));
            let decision = controller.evaluate(&safety_window(vec![required_lower_sample(
                metric,
                Some(100.0),
                Some(111.0),
            )]));

            assert_eq!(
                decision.action,
                RollbackAction::Rollback,
                "{}",
                metric.as_str()
            );
            assert_eq!(decision.mode, TuningMode::Rollback);
            assert_eq!(decision.rollback_value, Some(candidate.old_value));
            assert!(
                decision
                    .reason_codes
                    .contains(&metric_reason_code(metric, "regressed"))
            );
            assert!(
                decision
                    .reason_codes
                    .contains(&"auto_tune.rollback.metric_regression".to_string())
            );
        }
    }

    #[test]
    fn rollback_controller_uses_hysteresis_before_rollback() {
        let mut controller = RollbackController::new(RollbackControllerConfig {
            regression_hysteresis_windows: 2,
            cooldown_windows: 2,
            ..RollbackControllerConfig::default()
        });
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate.clone()));
        let regressed = safety_window(vec![required_lower_sample(
            SafetyMetricKind::QueueDepth,
            Some(100.0),
            Some(125.0),
        )]);
        let first = controller.evaluate(&regressed);

        assert_eq!(first.action, RollbackAction::ContinueCandidate);
        assert_eq!(first.regression_windows, 1);
        assert_eq!(controller.active_candidate(), Some(&candidate));
        assert!(
            first
                .reason_codes
                .contains(&"auto_tune.safety.regression_hysteresis".to_string())
        );

        let second = controller.evaluate(&regressed);
        assert_eq!(second.action, RollbackAction::Rollback);
        assert_eq!(second.rollback_value, Some(candidate.old_value));
        assert_eq!(second.cooldown_remaining_windows, 2);
        assert_eq!(controller.mode(), TuningMode::Cooldown);
    }

    #[test]
    fn rollback_controller_cooldown_blocks_immediate_reentry() {
        let mut controller = RollbackController::new(RollbackControllerConfig {
            regression_hysteresis_windows: 1,
            cooldown_windows: 1,
            ..RollbackControllerConfig::default()
        });
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate.clone()));
        let rollback = controller.evaluate(&safety_window(vec![required_lower_sample(
            SafetyMetricKind::MemoryPressure,
            Some(100.0),
            Some(150.0),
        )]));
        assert_eq!(rollback.action, RollbackAction::Rollback);

        assert!(!controller.start_candidate(candidate));
        assert_eq!(controller.active_candidate(), None);

        let cooldown = controller.evaluate(&safety_window(vec![required_lower_sample(
            SafetyMetricKind::MemoryPressure,
            Some(100.0),
            Some(100.0),
        )]));
        assert_eq!(cooldown.action, RollbackAction::Cooldown);
        assert_eq!(cooldown.cooldown_remaining_windows, 0);
        assert_eq!(controller.mode(), TuningMode::Observe);
    }

    #[test]
    fn rollback_controller_missing_required_telemetry_fails_closed() {
        let mut controller = RollbackController::new(RollbackControllerConfig {
            regression_hysteresis_windows: 1,
            cooldown_windows: 0,
            ..RollbackControllerConfig::default()
        });
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate.clone()));
        let empty = controller.evaluate(&safety_window(Vec::new()));
        assert_eq!(empty.action, RollbackAction::Rollback);
        assert_eq!(empty.rollback_value, Some(candidate.old_value));
        assert!(
            empty
                .reason_codes
                .contains(&"auto_tune.rollback.missing_telemetry".to_string())
        );

        assert!(controller.start_candidate(candidate));
        let required_missing = controller.evaluate(&safety_window(vec![required_lower_sample(
            SafetyMetricKind::DroppedWork,
            Some(100.0),
            None,
        )]));
        assert_eq!(required_missing.action, RollbackAction::Rollback);
        assert!(required_missing.reason_codes.contains(&metric_reason_code(
            SafetyMetricKind::DroppedWork,
            "missing_telemetry"
        )));
    }

    #[test]
    fn rollback_controller_optional_missing_telemetry_does_not_rollback() {
        let mut controller = RollbackController::new(RollbackControllerConfig::default());
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate.clone()));
        let decision = controller.evaluate(&safety_window(vec![
            required_lower_sample(SafetyMetricKind::ErrorRate, Some(100.0), Some(100.0)),
            optional_lower_sample(SafetyMetricKind::PolicyApprovalFailures, Some(100.0), None),
        ]));

        assert_eq!(decision.action, RollbackAction::AcceptCandidate);
        assert_eq!(
            controller.last_safe_value(candidate.knob_id),
            Some(candidate.candidate_value)
        );
    }

    #[test]
    fn rollback_controller_insufficient_confidence_rolls_back() {
        let mut controller = RollbackController::new(RollbackControllerConfig::default());
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate.clone()));
        let decision = controller.evaluate(&SafetyTelemetryWindow {
            confidence: 0.25,
            minimum_confidence: 0.80,
            samples: vec![required_lower_sample(
                SafetyMetricKind::Latency,
                Some(100.0),
                Some(100.0),
            )],
            ..SafetyTelemetryWindow::default()
        });

        assert_eq!(decision.action, RollbackAction::Rollback);
        assert_eq!(decision.rollback_value, Some(candidate.old_value));
        assert!(
            decision
                .reason_codes
                .contains(&"auto_tune.rollback.insufficient_confidence".to_string())
        );
    }

    #[test]
    fn rollback_controller_disable_clears_active_candidate() {
        let mut controller = RollbackController::new(RollbackControllerConfig::default());
        let candidate = candidate_for_rollback_controller();

        assert!(controller.start_candidate(candidate));
        assert!(controller.active_candidate().is_some());

        controller.disable();
        assert_eq!(controller.mode(), TuningMode::Disabled);
        assert_eq!(controller.active_candidate(), None);

        let decision = controller.evaluate(&safety_window(vec![required_lower_sample(
            SafetyMetricKind::Latency,
            Some(100.0),
            Some(100.0),
        )]));
        assert_eq!(decision.action, RollbackAction::Disabled);
        assert!(
            decision
                .reason_codes
                .contains(&"auto_tune.disabled".to_string())
        );
    }

    #[test]
    fn tuning_decision_records_cover_candidate_start_and_skipped_exploration_ft_luq3w_4() {
        let engine = fresh_observe_candidate_engine();
        let telemetry = pressure_window(CandidateDirection::Increase);
        let decision = engine.evaluate(&default_current_values(&engine), &telemetry, &[]);
        let record = TuningDecisionRecord::from_candidate_decision(
            1_700_000_030_000,
            "high-core-observe",
            "corr-auto-1",
            &decision,
            &telemetry,
        );

        assert_eq!(record.kind, TuningDecisionKind::CandidateStarted);
        assert_eq!(record.profile, "high-core-observe");
        assert_eq!(record.correlation_id, "corr-auto-1");
        assert_eq!(
            record.knob_id,
            Some(TunableKnobId::RuntimeOutputCoalesceWindowMs)
        );
        assert_eq!(
            record.knob_name.as_deref(),
            Some("runtime.output_coalesce_window_ms")
        );
        assert_eq!(record.old_value, Some(50.0));
        assert_eq!(record.new_value, Some(75.0));
        assert_eq!(
            record
                .metric_window
                .as_ref()
                .map(|window| window.confidence_state),
            Some(TuningConfidenceState::Acceptable)
        );
        assert!(
            record
                .reason_codes
                .contains(&"auto_tune.candidate.runtime.output_coalesce_window_ms".to_string())
        );

        let stale_engine = BoundedCandidateEngine::new(CandidateEvaluationConfig {
            telemetry_trust: TelemetryTrust::Stale,
            ..CandidateEvaluationConfig::default()
        });
        let skipped = stale_engine.evaluate(&default_current_values(&engine), &telemetry, &[]);
        let skipped_record = TuningDecisionRecord::from_candidate_decision(
            1_700_000_030_001,
            "high-core-observe",
            "corr-auto-2",
            &skipped,
            &telemetry,
        );

        assert_eq!(skipped_record.kind, TuningDecisionKind::ExplorationSkipped);
        assert_eq!(skipped_record.knob_id, None);
        assert_eq!(
            skipped_record.reason_codes,
            vec!["auto_tune.skipped.stale_telemetry".to_string()]
        );
    }

    #[test]
    fn tuning_decision_log_records_candidate_rejection_and_rollback_bounded_ft_luq3w_4() {
        let mut controller = RollbackController::new(RollbackControllerConfig {
            regression_hysteresis_windows: 1,
            ..RollbackControllerConfig::default()
        });
        let candidate = candidate_for_rollback_controller();
        assert!(controller.start_candidate(candidate.clone()));

        let safety = safety_window(vec![required_lower_sample(
            SafetyMetricKind::Latency,
            Some(100.0),
            Some(150.0),
        )]);
        let rollback = controller.evaluate(&safety);

        let mut log = TuningDecisionLog::new(2);
        let telemetry = pressure_window(CandidateDirection::Increase);
        let candidate_decision = CandidateDecision {
            mode: TuningMode::Observe,
            candidate: Some(candidate),
            skip_reasons: Vec::new(),
            active_explorations: 0,
            max_concurrent_explorations: 1,
        };
        log.record_candidate_decision(
            1_700_000_030_000,
            "high-core-canary",
            "corr-auto-3",
            &candidate_decision,
            &telemetry,
        );
        log.record_rollback_decision(
            1_700_000_030_010,
            "high-core-canary",
            "corr-auto-3",
            &rollback,
            Some(&safety),
        );

        let records = log.recent();
        assert_eq!(log.capacity(), 2);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, TuningDecisionKind::CandidateRejected);
        assert_eq!(records[1].kind, TuningDecisionKind::Rollback);
        assert_eq!(records[1].rollback_value, Some(50.0));
        assert!(
            records[1]
                .reason_codes
                .contains(&"auto_tune.rollback.metric_regression".to_string())
        );
        assert_eq!(records[1].safety_checks.len(), 1);
        assert_eq!(
            records[1].safety_checks[0].reason_code,
            "auto_tune.safety.latency.regressed"
        );
    }

    #[test]
    fn auto_tune_replay_fixed_trace_accepts_candidate_ft_luq3w_5() {
        let trace = replay_trace(
            "ft-luq3w.5.accept",
            vec![replay_step(
                "corr-replay-accept",
                TelemetryTrust::Fresh,
                vec![safety_window(vec![required_lower_sample(
                    SafetyMetricKind::Latency,
                    Some(100.0),
                    Some(101.0),
                )])],
            )],
        );

        let report = run_auto_tune_replay_trace(&trace);
        let knob = TunableKnobId::RuntimeOutputCoalesceWindowMs;

        assert_eq!(report.schema_version, AUTO_TUNE_REPLAY_PROOF_SCHEMA_VERSION);
        assert_eq!(report.summary.steps, 1);
        assert_eq!(report.summary.candidates_started, 1);
        assert_eq!(report.summary.candidates_accepted, 1);
        assert_eq!(report.summary.candidates_rejected, 0);
        assert_eq!(report.summary.rollbacks, 0);
        assert!(report.summary.accepted_candidates_preserved_or_improved);
        assert!(report.summary.regressed_candidates_rolled_back);
        assert_eq!(report.before_values.get(&knob).copied(), Some(50.0));
        assert_eq!(report.after_values.get(&knob).copied(), Some(75.0));
        assert_eq!(
            report
                .decisions
                .iter()
                .map(|record| record.kind)
                .collect::<Vec<_>>(),
            vec![
                TuningDecisionKind::CandidateStarted,
                TuningDecisionKind::CandidateAccepted,
            ]
        );
        assert_eq!(report.decisions[1].safety_checks.len(), 1);
        assert_eq!(
            report.decisions[1].safety_checks[0].reason_code,
            "auto_tune.safety.latency.healthy"
        );
    }

    #[test]
    fn auto_tune_replay_fixed_trace_rejects_and_rolls_back_ft_luq3w_5() {
        let trace = replay_trace(
            "ft-luq3w.5.rollback",
            vec![replay_step(
                "corr-replay-rollback",
                TelemetryTrust::Fresh,
                vec![safety_window(vec![required_lower_sample(
                    SafetyMetricKind::Latency,
                    Some(100.0),
                    Some(150.0),
                )])],
            )],
        );

        let report = run_auto_tune_replay_trace(&trace);
        let knob = TunableKnobId::RuntimeOutputCoalesceWindowMs;

        assert_eq!(report.summary.candidates_started, 1);
        assert_eq!(report.summary.candidates_accepted, 0);
        assert_eq!(report.summary.candidates_rejected, 1);
        assert_eq!(report.summary.rollbacks, 1);
        assert!(report.summary.regressed_candidates_rolled_back);
        assert_eq!(report.before_values.get(&knob).copied(), Some(50.0));
        assert_eq!(report.after_values.get(&knob).copied(), Some(50.0));
        assert_eq!(
            report
                .decisions
                .iter()
                .map(|record| record.kind)
                .collect::<Vec<_>>(),
            vec![
                TuningDecisionKind::CandidateStarted,
                TuningDecisionKind::CandidateRejected,
                TuningDecisionKind::Rollback,
            ]
        );
        assert!(
            report.decisions[2]
                .reason_codes
                .contains(&"auto_tune.rollback.metric_regression".to_string())
        );
        assert_eq!(report.decisions[2].rollback_value, Some(50.0));
    }

    #[test]
    fn auto_tune_replay_fixed_trace_skips_missing_and_stale_telemetry_ft_luq3w_5() {
        let trace = replay_trace(
            "ft-luq3w.5.noop",
            vec![
                replay_step("corr-replay-missing", TelemetryTrust::Missing, Vec::new()),
                replay_step("corr-replay-stale", TelemetryTrust::Stale, Vec::new()),
            ],
        );

        let report = run_auto_tune_replay_trace(&trace);

        assert_eq!(report.summary.steps, 2);
        assert_eq!(report.summary.candidates_started, 0);
        assert_eq!(report.summary.explorations_skipped, 2);
        assert_eq!(report.summary.missing_or_stale_telemetry_noops, 2);
        assert_eq!(report.before_values, report.after_values);
        assert_eq!(
            report
                .decisions
                .iter()
                .map(|record| record.kind)
                .collect::<Vec<_>>(),
            vec![
                TuningDecisionKind::ExplorationSkipped,
                TuningDecisionKind::ExplorationSkipped,
            ]
        );
        assert_eq!(
            report.decisions[0].reason_codes,
            vec!["auto_tune.skipped.missing_telemetry".to_string()]
        );
        assert_eq!(
            report.decisions[1].reason_codes,
            vec!["auto_tune.skipped.stale_telemetry".to_string()]
        );
    }

    #[test]
    fn auto_tune_replay_report_is_deterministic_and_high_scale_unproven_ft_luq3w_5() {
        let trace = replay_trace(
            "ft-luq3w.5.deterministic",
            vec![replay_step(
                "corr-replay-deterministic",
                TelemetryTrust::Fresh,
                vec![safety_window(vec![required_lower_sample(
                    SafetyMetricKind::Latency,
                    Some(100.0),
                    Some(100.0),
                )])],
            )],
        );

        let first = run_auto_tune_replay_trace(&trace);
        let second = run_auto_tune_replay_trace(&trace);

        assert_eq!(first, second);
        assert_eq!(first.evidence_level, AutoTuneEvidenceLevel::LocalReduced);
        assert_eq!(
            first.high_scale_evidence_level,
            AutoTuneEvidenceLevel::SkippedNotProven
        );
        assert!(!first.high_scale_claim_allowed);
        assert_eq!(
            first.high_scale_reason_code,
            "auto_tune.proof.high_scale.skipped_not_proven"
        );
        assert_eq!(first.artifact_paths, vec!["inline-fixed-trace".to_string()]);
    }

    // ---- Hysteresis tests ----

    #[test]
    fn hysteresis_prevents_immediate_change() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 3,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().clone();

        // Only 2 ticks of high memory — should not change (need 3)
        tuner.tick(&high_memory_metrics());
        tuner.tick(&high_memory_metrics());

        let p = tuner.params();
        assert!((p.poll_interval_ms - initial.poll_interval_ms).abs() < f64::EPSILON);
        assert!((p.scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON);
        assert!((p.snapshot_interval_secs - initial.snapshot_interval_secs).abs() < f64::EPSILON);
        assert!((p.pool_size - initial.pool_size).abs() < f64::EPSILON);
        assert!((p.backpressure_threshold - initial.backpressure_threshold).abs() < f64::EPSILON);
    }

    #[test]
    fn hysteresis_allows_change_after_sustained_signal() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 3,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial_scrollback = tuner.params().scrollback_lines;

        // 3+ ticks of high memory → should reduce scrollback
        for _ in 0..5 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(tuner.params().scrollback_lines < initial_scrollback);
    }

    #[test]
    fn hysteresis_resets_on_direction_change() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 3,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().clone();

        // 2 ticks high, then calm — resets counter
        tuner.tick(&high_memory_metrics());
        tuner.tick(&high_memory_metrics());
        tuner.tick(&calm_metrics());
        tuner.tick(&high_memory_metrics());

        // Should not have changed (hysteresis reset)
        assert!((tuner.params().scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON);
    }

    // ---- Gradual change tests ----

    #[test]
    fn max_change_per_tick_limits_adjustment() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1, // immediate response for testing
            max_change_per_tick: 0.1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial_scrollback = tuner.params().scrollback_lines;

        // Very high memory pressure
        let extreme = TunerMetrics {
            rss_fraction: 0.95,
            mux_latency_ms: 5.0,
            cpu_fraction: 0.15,
        };
        tuner.tick(&extreme);

        let change_ratio =
            (initial_scrollback - tuner.params().scrollback_lines) / initial_scrollback;
        // Should not exceed 10% change
        assert!(change_ratio <= 0.1 + f64::EPSILON);
    }

    // ---- Memory pressure tests ----

    #[test]
    fn memory_pressure_reduces_scrollback() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().scrollback_lines;

        for _ in 0..5 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(tuner.params().scrollback_lines < initial);
    }

    #[test]
    fn memory_pressure_increases_snapshot_interval() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().snapshot_interval_secs;

        for _ in 0..5 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(tuner.params().snapshot_interval_secs > initial);
    }

    // ---- Latency pressure tests ----

    #[test]
    fn latency_pressure_increases_poll_interval() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().poll_interval_ms;

        for _ in 0..5 {
            tuner.tick(&high_latency_metrics());
        }

        assert!(tuner.params().poll_interval_ms > initial);
    }

    // ---- CPU pressure tests ----

    #[test]
    fn cpu_pressure_reduces_pool_size() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().pool_size;

        for _ in 0..5 {
            tuner.tick(&high_cpu_metrics());
        }

        assert!(tuner.params().pool_size < initial);
    }

    // ---- Pinned parameter tests ----

    #[test]
    fn pinned_scrollback_not_modified() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        tuner.set_pinned(PinnedParams {
            scrollback_lines: true,
            ..PinnedParams::default()
        });
        let initial = tuner.params().scrollback_lines;

        for _ in 0..10 {
            tuner.tick(&high_memory_metrics());
        }

        assert!((tuner.params().scrollback_lines - initial).abs() < f64::EPSILON);
    }

    #[test]
    fn pinned_poll_interval_not_modified() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        tuner.set_pinned(PinnedParams {
            poll_interval_ms: true,
            ..PinnedParams::default()
        });
        let initial = tuner.params().poll_interval_ms;

        for _ in 0..10 {
            tuner.tick(&high_latency_metrics());
        }

        assert!((tuner.params().poll_interval_ms - initial).abs() < f64::EPSILON);
    }

    #[test]
    fn pinned_pool_size_not_modified() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        tuner.set_pinned(PinnedParams {
            pool_size: true,
            ..PinnedParams::default()
        });
        let initial = tuner.params().pool_size;

        for _ in 0..10 {
            tuner.tick(&high_cpu_metrics());
        }

        assert!((tuner.params().pool_size - initial).abs() < f64::EPSILON);
    }

    // ---- Adjustment log tests ----

    #[test]
    fn adjustments_logged() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);

        for _ in 0..5 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(!tuner.adjustments().is_empty());
        assert!(
            tuner
                .adjustments()
                .iter()
                .any(|a| a.param == "scrollback_lines")
        );
    }

    #[test]
    fn clear_adjustments() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);

        for _ in 0..5 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(!tuner.adjustments().is_empty());
        tuner.clear_adjustments();
        assert!(tuner.adjustments().is_empty());
    }

    // ---- Tick count ----

    #[test]
    fn tick_count_increments() {
        let mut tuner = AutoTuner::new(default_config());
        assert_eq!(tuner.tick_count(), 0);

        tuner.tick(&calm_metrics());
        assert_eq!(tuner.tick_count(), 1);

        tuner.tick(&calm_metrics());
        assert_eq!(tuner.tick_count(), 2);
    }

    // ---- History bounded ----

    #[test]
    fn history_bounded() {
        let config = AutoTuneConfig {
            history_limit: 5,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);

        for _ in 0..20 {
            tuner.tick(&calm_metrics());
        }

        assert!(tuner.history.len() <= 5);
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn with_params_sets_initial_params() {
        let custom = TunableParams {
            poll_interval_ms: 500.0,
            scrollback_lines: 2000.0,
            snapshot_interval_secs: 120.0,
            pool_size: 8.0,
            backpressure_threshold: 0.6,
        };
        let tuner = AutoTuner::with_params(default_config(), custom.clone());
        let p = tuner.params();
        assert!(
            (p.poll_interval_ms - 500.0).abs() < f64::EPSILON,
            "poll_interval_ms: {}",
            p.poll_interval_ms
        );
        assert!(
            (p.scrollback_lines - 2000.0).abs() < f64::EPSILON,
            "scrollback_lines: {}",
            p.scrollback_lines
        );
        assert!(
            (p.snapshot_interval_secs - 120.0).abs() < f64::EPSILON,
            "snapshot_interval_secs: {}",
            p.snapshot_interval_secs
        );
        assert!(
            (p.pool_size - 8.0).abs() < f64::EPSILON,
            "pool_size: {}",
            p.pool_size
        );
        assert!(
            (p.backpressure_threshold - 0.6).abs() < f64::EPSILON,
            "backpressure_threshold: {}",
            p.backpressure_threshold
        );
        assert_eq!(tuner.tick_count(), 0);
        assert!(tuner.adjustments().is_empty());
    }

    #[test]
    fn param_range_clamp_at_min() {
        let range = ParamRange {
            min: 10.0,
            max: 100.0,
        };
        assert!((range.clamp(10.0) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn param_range_clamp_at_max() {
        let range = ParamRange {
            min: 10.0,
            max: 100.0,
        };
        assert!((range.clamp(100.0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn param_range_clamp_in_range() {
        let range = ParamRange {
            min: 10.0,
            max: 100.0,
        };
        assert!((range.clamp(55.0) - 55.0).abs() < f64::EPSILON);
    }

    #[test]
    fn param_range_clamp_below_min() {
        let range = ParamRange {
            min: 10.0,
            max: 100.0,
        };
        assert!((range.clamp(-5.0) - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn param_range_clamp_above_max() {
        let range = ParamRange {
            min: 10.0,
            max: 100.0,
        };
        assert!((range.clamp(999.0) - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn auto_tune_config_default_values() {
        let cfg = AutoTuneConfig::default();
        assert!(
            !cfg.enabled,
            "auto-tuning must stay disabled unless an operator explicitly enables it"
        );
        assert_eq!(cfg.tick_interval_secs, 30);
        assert!((cfg.max_change_per_tick - 0.1).abs() < f64::EPSILON);
        assert_eq!(cfg.hysteresis_ticks, 3);
        assert_eq!(cfg.history_limit, 100);
    }

    #[test]
    fn disabled_config_observes_history_without_mutating_params_ft_luq3w_6() {
        let mut tuner = AutoTuner::new(AutoTuneConfig {
            hysteresis_ticks: 1,
            ..AutoTuneConfig::default()
        });
        let initial = tuner.params().clone();

        for _ in 0..5 {
            tuner.tick(&high_memory_metrics());
            tuner.tick(&high_latency_metrics());
            tuner.tick(&high_cpu_metrics());
        }

        assert_eq!(tuner.tick_count(), 15);
        assert_eq!(tuner.history.len(), 15);
        assert!(tuner.adjustments().is_empty());
        assert_eq!(tuner.params(), &initial);
    }

    #[test]
    fn tuning_targets_default_values() {
        let t = TuningTargets::default();
        assert!((t.target_rss_fraction - 0.5).abs() < f64::EPSILON);
        assert!((t.target_latency_ms - 10.0).abs() < f64::EPSILON);
        assert!((t.target_cpu_fraction - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn pinned_params_default_all_false() {
        let p = PinnedParams::default();
        assert!(!p.poll_interval_ms);
        assert!(!p.scrollback_lines);
        assert!(!p.snapshot_interval_secs);
        assert!(!p.pool_size);
        assert!(!p.backpressure_threshold);
    }

    #[test]
    fn tunable_params_serde_roundtrip() {
        let params = TunableParams {
            poll_interval_ms: 350.0,
            scrollback_lines: 7500.0,
            snapshot_interval_secs: 180.0,
            pool_size: 6.0,
            backpressure_threshold: 0.55,
        };
        let json = serde_json::to_string(&params).unwrap();
        let deserialized: TunableParams = serde_json::from_str(&json).unwrap();
        assert_eq!(params, deserialized);
    }

    #[test]
    fn auto_tune_config_serde_roundtrip() {
        let cfg = AutoTuneConfig {
            enabled: false,
            tick_interval_secs: 60,
            targets: TuningTargets {
                target_rss_fraction: 0.4,
                target_latency_ms: 20.0,
                target_cpu_fraction: 0.5,
            },
            max_change_per_tick: 0.2,
            hysteresis_ticks: 5,
            history_limit: 50,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: AutoTuneConfig = serde_json::from_str(&json).unwrap();
        assert!(!deserialized.enabled);
        assert_eq!(deserialized.tick_interval_secs, 60);
        assert!((deserialized.max_change_per_tick - 0.2).abs() < f64::EPSILON);
        assert_eq!(deserialized.hysteresis_ticks, 5);
        assert_eq!(deserialized.history_limit, 50);
        assert!((deserialized.targets.target_rss_fraction - 0.4).abs() < f64::EPSILON);
        assert!((deserialized.targets.target_latency_ms - 20.0).abs() < f64::EPSILON);
        assert!((deserialized.targets.target_cpu_fraction - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn low_memory_pressure_restores_scrollback() {
        // When rss_fraction is low (pressure < 0.95), scrollback should increase
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        // Start with reduced scrollback
        let params = TunableParams {
            scrollback_lines: 2000.0,
            ..TunableParams::default()
        };
        let mut tuner = AutoTuner::with_params(config, params);
        let initial = tuner.params().scrollback_lines;

        // Very low memory usage: rss_fraction=0.1 => pressure = 0.1/0.5 = 0.2 (< 0.95)
        let low_mem = TunerMetrics {
            rss_fraction: 0.1,
            mux_latency_ms: 10.0,
            cpu_fraction: 0.3,
        };
        for _ in 0..5 {
            tuner.tick(&low_mem);
        }

        assert!(
            tuner.params().scrollback_lines > initial,
            "scrollback should increase when memory pressure is low: initial={}, got={}",
            initial,
            tuner.params().scrollback_lines
        );
    }

    #[test]
    fn low_latency_decreases_poll_interval() {
        // When latency is low (pressure < 0.95), poll interval should decrease
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        // Start with elevated poll interval
        let params = TunableParams {
            poll_interval_ms: 1000.0,
            ..TunableParams::default()
        };
        let mut tuner = AutoTuner::with_params(config, params);
        let initial = tuner.params().poll_interval_ms;

        // Very low latency: 2ms => pressure = 2/10 = 0.2 (< 0.95)
        let low_lat = TunerMetrics {
            rss_fraction: 0.5,
            mux_latency_ms: 2.0,
            cpu_fraction: 0.3,
        };
        for _ in 0..5 {
            tuner.tick(&low_lat);
        }

        assert!(
            tuner.params().poll_interval_ms < initial,
            "poll_interval should decrease when latency is low: initial={}, got={}",
            initial,
            tuner.params().poll_interval_ms
        );
    }

    #[test]
    fn low_cpu_restores_pool_size() {
        // When cpu_fraction is low (pressure < 0.95), pool_size should increase
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        // Start with reduced pool size
        let params = TunableParams {
            pool_size: 2.0,
            ..TunableParams::default()
        };
        let mut tuner = AutoTuner::with_params(config, params);
        let initial = tuner.params().pool_size;

        // Very low CPU: 0.05 => pressure = 0.05/0.3 = 0.167 (< 0.95)
        let low_cpu = TunerMetrics {
            rss_fraction: 0.5,
            mux_latency_ms: 10.0,
            cpu_fraction: 0.05,
        };
        for _ in 0..5 {
            tuner.tick(&low_cpu);
        }

        assert!(
            tuner.params().pool_size > initial,
            "pool_size should increase when CPU is low: initial={}, got={}",
            initial,
            tuner.params().pool_size
        );
    }

    #[test]
    fn multiple_concurrent_pressures() {
        // High memory + high latency + high CPU simultaneously
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().clone();

        let all_high = TunerMetrics {
            rss_fraction: 0.85,   // high memory
            mux_latency_ms: 30.0, // high latency
            cpu_fraction: 0.7,    // high CPU
        };

        for _ in 0..10 {
            tuner.tick(&all_high);
        }

        let p = tuner.params();
        // Memory pressure: scrollback should decrease
        assert!(
            p.scrollback_lines < initial.scrollback_lines,
            "scrollback should decrease under memory pressure"
        );
        // Memory pressure: snapshot interval should increase
        assert!(
            p.snapshot_interval_secs > initial.snapshot_interval_secs,
            "snapshot_interval should increase under memory pressure"
        );
        // Latency + CPU pressure: poll interval should increase
        assert!(
            p.poll_interval_ms > initial.poll_interval_ms,
            "poll_interval should increase under latency+CPU pressure"
        );
        // CPU pressure: pool size should decrease
        assert!(
            p.pool_size < initial.pool_size,
            "pool_size should decrease under CPU pressure"
        );
    }

    #[test]
    fn pinned_snapshot_interval_not_modified_under_memory_pressure() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        tuner.set_pinned(PinnedParams {
            snapshot_interval_secs: true,
            ..PinnedParams::default()
        });
        let initial = tuner.params().snapshot_interval_secs;

        for _ in 0..10 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(
            (tuner.params().snapshot_interval_secs - initial).abs() < f64::EPSILON,
            "pinned snapshot_interval_secs should not change: initial={}, got={}",
            initial,
            tuner.params().snapshot_interval_secs
        );
    }

    #[test]
    fn pinned_backpressure_threshold_not_modified() {
        // backpressure_threshold is never directly adjusted by the tuner,
        // but verify pinning it doesn't cause issues and it stays unchanged
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        tuner.set_pinned(PinnedParams {
            backpressure_threshold: true,
            ..PinnedParams::default()
        });
        let initial = tuner.params().backpressure_threshold;

        let all_high = TunerMetrics {
            rss_fraction: 0.9,
            mux_latency_ms: 50.0,
            cpu_fraction: 0.8,
        };
        for _ in 0..10 {
            tuner.tick(&all_high);
        }

        assert!(
            (tuner.params().backpressure_threshold - initial).abs() < f64::EPSILON,
            "pinned backpressure_threshold should not change"
        );
    }

    #[test]
    fn apply_gradual_change_delta_at_max_boundary() {
        // When the requested change is exactly at the max boundary,
        // the target should be reached immediately.
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            max_change_per_tick: 0.1,
            ..default_config()
        };
        let tuner = AutoTuner::new(config);

        // current=100, target=110 => delta=10, max_delta=100*0.1=10 => delta==max_delta
        let result = tuner.apply_gradual_change(100.0, 110.0);
        assert!(
            (result - 110.0).abs() < f64::EPSILON,
            "should reach target when delta equals max: got {}",
            result
        );

        // current=100, target=90 => delta=-10, |delta|=10 = max_delta
        let result_down = tuner.apply_gradual_change(100.0, 90.0);
        assert!(
            (result_down - 90.0).abs() < f64::EPSILON,
            "should reach target when negative delta equals max: got {}",
            result_down
        );
    }

    #[test]
    fn very_high_max_change_allows_immediate_convergence() {
        // With max_change_per_tick = 1.0 (100%), one tick should fully converge
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            max_change_per_tick: 1.0,
            ..default_config()
        };
        let tuner = AutoTuner::new(config);

        // current=200, target=50 => delta=-150, max_delta=200*1.0=200 => |delta|<max_delta
        let result = tuner.apply_gradual_change(200.0, 50.0);
        assert!(
            (result - 50.0).abs() < f64::EPSILON,
            "100% max_change should allow full convergence: got {}",
            result
        );
    }

    #[test]
    fn zero_hysteresis_ticks_immediate_response() {
        // hysteresis_ticks=0 means the first tick of pressure triggers a change
        // (threshold=0 => ticks(1) >= 0 is always true)
        let config = AutoTuneConfig {
            hysteresis_ticks: 0,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);
        let initial = tuner.params().scrollback_lines;

        // A single tick of high memory should already change scrollback
        tuner.tick(&high_memory_metrics());
        assert!(
            tuner.params().scrollback_lines < initial,
            "with hysteresis=0, first tick should trigger change: initial={}, got={}",
            initial,
            tuner.params().scrollback_lines
        );
    }

    #[test]
    fn history_limit_one_still_works() {
        let config = AutoTuneConfig {
            history_limit: 1,
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);

        for _ in 0..10 {
            tuner.tick(&high_memory_metrics());
        }

        assert!(tuner.history.len() <= 1);
        assert_eq!(tuner.tick_count(), 10);
        // Tuner should still function with minimal history
        assert!(tuner.params().scrollback_lines < TunableParams::default().scrollback_lines);
    }

    #[test]
    fn adjustment_records_contain_correct_param_and_reason() {
        let config = AutoTuneConfig {
            hysteresis_ticks: 1,
            ..default_config()
        };
        let mut tuner = AutoTuner::new(config);

        // Trigger memory adjustments
        for _ in 0..3 {
            tuner.tick(&high_memory_metrics());
        }

        let mem_adjs: Vec<&Adjustment> = tuner
            .adjustments()
            .iter()
            .filter(|a| a.reason == "memory pressure")
            .collect();
        assert!(
            !mem_adjs.is_empty(),
            "should have memory pressure adjustments"
        );
        for adj in &mem_adjs {
            assert!(
                adj.param == "scrollback_lines" || adj.param == "snapshot_interval_secs",
                "unexpected param under memory pressure: {}",
                adj.param
            );
            assert!(adj.pressure > 1.0, "memory pressure ratio should be > 1.0");
            assert!(
                (adj.old_value - adj.new_value).abs() > f64::EPSILON,
                "old and new values should differ"
            );
        }

        // Now trigger CPU adjustments
        tuner.clear_adjustments();
        for _ in 0..3 {
            tuner.tick(&high_cpu_metrics());
        }

        let cpu_adjs: Vec<&Adjustment> = tuner
            .adjustments()
            .iter()
            .filter(|a| a.reason == "CPU pressure")
            .collect();
        for adj in &cpu_adjs {
            assert_eq!(adj.param, "pool_size");
            assert!(adj.pressure > 1.0);
        }
    }

    #[test]
    fn tick_count_after_mixed_ticks() {
        let mut tuner = AutoTuner::new(default_config());
        assert_eq!(tuner.tick_count(), 0);

        tuner.tick(&calm_metrics());
        tuner.tick(&high_memory_metrics());
        tuner.tick(&high_latency_metrics());
        tuner.tick(&high_cpu_metrics());
        tuner.tick(&calm_metrics());

        assert_eq!(
            tuner.tick_count(),
            5,
            "tick_count should be 5 after 5 mixed ticks"
        );
    }

    #[test]
    fn integer_getters_with_non_default_values() {
        // Test rounding behavior of integer getters
        let params = TunableParams {
            poll_interval_ms: 150.4,
            scrollback_lines: 3500.6,
            snapshot_interval_secs: 90.5,
            pool_size: 5.7,
            backpressure_threshold: 0.8,
        };
        // 150.4 rounds to 150
        assert_eq!(params.poll_interval_ms_u64(), 150);
        // 3500.6 rounds to 3501
        assert_eq!(params.scrollback_lines_usize(), 3501);
        // 90.5 rounds to 91 (banker's rounding: .5 rounds to even... actually f64::round
        // rounds away from zero for .5)
        assert_eq!(params.snapshot_interval_secs_u64(), 91);
        // 5.7 rounds to 6
        assert_eq!(params.pool_size_usize(), 6);
    }

    // ---- proptest ----

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn arb_metrics() -> impl Strategy<Value = TunerMetrics> {
            (0.0..=1.0_f64, 0.1..=100.0_f64, 0.0..=1.0_f64).prop_map(|(rss, latency, cpu)| {
                TunerMetrics {
                    rss_fraction: rss,
                    mux_latency_ms: latency,
                    cpu_fraction: cpu,
                }
            })
        }

        proptest! {
            /// For any sequence of metrics, all output parameters remain within ranges.
            #[test]
            fn range_invariant(
                metrics in proptest::collection::vec(arb_metrics(), 1..=50)
            ) {
                let config = AutoTuneConfig {
                    hysteresis_ticks: 1,
                    ..default_config()
                };
                let mut tuner = AutoTuner::new(config);

                for m in &metrics {
                    let params = tuner.tick(m);
                    prop_assert!(params.poll_interval_ms >= POLL_INTERVAL_RANGE.min);
                    prop_assert!(params.poll_interval_ms <= POLL_INTERVAL_RANGE.max);
                    prop_assert!(params.scrollback_lines >= SCROLLBACK_LINES_RANGE.min);
                    prop_assert!(params.scrollback_lines <= SCROLLBACK_LINES_RANGE.max);
                    prop_assert!(params.snapshot_interval_secs >= SNAPSHOT_INTERVAL_RANGE.min);
                    prop_assert!(params.snapshot_interval_secs <= SNAPSHOT_INTERVAL_RANGE.max);
                    prop_assert!(params.pool_size >= POOL_SIZE_RANGE.min);
                    prop_assert!(params.pool_size <= POOL_SIZE_RANGE.max);
                    prop_assert!(params.backpressure_threshold >= BACKPRESSURE_THRESHOLD_RANGE.min);
                    prop_assert!(params.backpressure_threshold <= BACKPRESSURE_THRESHOLD_RANGE.max);
                }
            }

            /// Constant metrics over many ticks -> parameters converge.
            /// When competing pressures act on the same parameter (e.g., CPU pushes
            /// poll_interval up while latency pushes it down), the parameter drifts
            /// toward a range bound. We run 1000 ticks so even slow drift (~0.4%/tick)
            /// reaches equilibrium. Threshold of 50 covers the worst case: poll_interval
            /// approaching its 10000 upper bound at ~0.39%/tick yields ~39 change/tick.
            #[test]
            fn convergence_on_constant_input(
                rss in 0.0..=1.0_f64,
                latency in 0.1..=100.0_f64,
                cpu in 0.0..=1.0_f64,
            ) {
                let config = AutoTuneConfig {
                    hysteresis_ticks: 1,
                    ..default_config()
                };
                let mut tuner = AutoTuner::new(config);
                let metrics = TunerMetrics {
                    rss_fraction: rss,
                    mux_latency_ms: latency,
                    cpu_fraction: cpu,
                };

                // Run 1000 ticks -- enough for competing-pressure drift to hit bounds
                let mut prev = tuner.params().clone();
                let mut last_change = f64::MAX;
                for _ in 0..1000 {
                    let current = tuner.tick(&metrics);
                    let change = (current.poll_interval_ms - prev.poll_interval_ms).abs()
                        + (current.scrollback_lines - prev.scrollback_lines).abs()
                        + (current.snapshot_interval_secs - prev.snapshot_interval_secs).abs()
                        + (current.pool_size - prev.pool_size).abs();
                    last_change = change;
                    prev = current;
                }

                prop_assert!(last_change < 50.0,
                    "After 1000 ticks of constant input, change per tick should approach zero, got: {last_change}");
            }

            /// Monotonic memory pressure → scrollback decreases monotonically.
            #[test]
            fn monotonic_memory_response(
                base_rss in 0.6..=0.9_f64,
            ) {
                let config = AutoTuneConfig {
                    hysteresis_ticks: 1,
                    max_change_per_tick: 0.1,
                    ..default_config()
                };
                let mut tuner = AutoTuner::new(config);

                // Apply sustained pressure with increasing RSS
                let mut prev_scrollback = tuner.params().scrollback_lines;
                for i in 0..20 {
                    let rss = (i as f64).mul_add(0.005, base_rss);
                    let metrics = TunerMetrics {
                        rss_fraction: rss.min(1.0),
                        mux_latency_ms: 5.0,
                        cpu_fraction: 0.15,
                    };
                    tuner.tick(&metrics);
                    let current_scrollback = tuner.params().scrollback_lines;
                    // Scrollback should decrease or stay the same (never increase
                    // under monotonically increasing memory pressure)
                    prop_assert!(current_scrollback <= prev_scrollback + f64::EPSILON,
                        "Scrollback should not increase under rising memory pressure: prev={prev_scrollback}, current={current_scrollback}");
                    prev_scrollback = current_scrollback;
                }
            }

            /// Pinned parameters are never modified.
            #[test]
            fn pinned_params_respected(
                metrics in proptest::collection::vec(arb_metrics(), 1..=30)
            ) {
                let config = AutoTuneConfig {
                    hysteresis_ticks: 1,
                    ..default_config()
                };
                let mut tuner = AutoTuner::new(config);
                tuner.set_pinned(PinnedParams {
                    poll_interval_ms: true,
                    scrollback_lines: true,
                    snapshot_interval_secs: true,
                    pool_size: true,
                    backpressure_threshold: true,
                });
                let initial = tuner.params().clone();

                for m in &metrics {
                    tuner.tick(m);
                    prop_assert!((tuner.params().poll_interval_ms - initial.poll_interval_ms).abs() < f64::EPSILON,
                        "poll_interval_ms changed: {} vs {}", tuner.params().poll_interval_ms, initial.poll_interval_ms);
                    prop_assert!((tuner.params().scrollback_lines - initial.scrollback_lines).abs() < f64::EPSILON,
                        "scrollback_lines changed: {} vs {}", tuner.params().scrollback_lines, initial.scrollback_lines);
                    prop_assert!((tuner.params().snapshot_interval_secs - initial.snapshot_interval_secs).abs() < f64::EPSILON,
                        "snapshot_interval_secs changed: {} vs {}", tuner.params().snapshot_interval_secs, initial.snapshot_interval_secs);
                    prop_assert!((tuner.params().pool_size - initial.pool_size).abs() < f64::EPSILON,
                        "pool_size changed: {} vs {}", tuner.params().pool_size, initial.pool_size);
                    prop_assert!((tuner.params().backpressure_threshold - initial.backpressure_threshold).abs() < f64::EPSILON,
                        "backpressure_threshold changed: {} vs {}", tuner.params().backpressure_threshold, initial.backpressure_threshold);
                }
            }
        }
    }
}
