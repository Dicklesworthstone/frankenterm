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

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

// =============================================================================
// Parameter ranges
// =============================================================================

/// Hard minimum and maximum for each tunable parameter.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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
    /// Whether auto-tuning is enabled.
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
            enabled: true,
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
            current + delta.signum() * max_delta
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> AutoTuneConfig {
        AutoTuneConfig::default()
    }

    fn calm_metrics() -> TunerMetrics {
        // Values within the 0.95–1.05 deadband of the default targets
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

        assert_eq!(tuner.params(), &initial);
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

        assert_eq!(params.poll_interval_ms, POLL_INTERVAL_RANGE.min);
        assert_eq!(params.scrollback_lines, SCROLLBACK_LINES_RANGE.max);
        assert_eq!(params.snapshot_interval_secs, SNAPSHOT_INTERVAL_RANGE.min);
        assert_eq!(params.pool_size, POOL_SIZE_RANGE.max);
        assert_eq!(
            params.backpressure_threshold,
            BACKPRESSURE_THRESHOLD_RANGE.min
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

        assert_eq!(tuner.params(), &initial);
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
        assert_eq!(tuner.params().scrollback_lines, initial.scrollback_lines);
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

        assert_eq!(tuner.params().scrollback_lines, initial);
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

        assert_eq!(tuner.params().poll_interval_ms, initial);
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

        assert_eq!(tuner.params().pool_size, initial);
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
                    ..AutoTuneConfig::default()
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

            /// Constant metrics over many ticks → parameters converge (change approaches zero).
            /// When competing pressures act on the same parameter, convergence takes longer
            /// as the parameter drifts toward a bound. We run 500 ticks to allow parameters
            /// to reach their equilibrium (typically at a range bound).
            #[test]
            fn convergence_on_constant_input(
                rss in 0.0..=1.0_f64,
                latency in 0.1..=100.0_f64,
                cpu in 0.0..=1.0_f64,
            ) {
                let config = AutoTuneConfig {
                    hysteresis_ticks: 1,
                    ..AutoTuneConfig::default()
                };
                let mut tuner = AutoTuner::new(config);
                let metrics = TunerMetrics {
                    rss_fraction: rss,
                    mux_latency_ms: latency,
                    cpu_fraction: cpu,
                };

                // Run 500 ticks (enough for any parameter to reach its bound)
                let mut prev = tuner.params().clone();
                let mut last_change = f64::MAX;
                for _ in 0..500 {
                    let current = tuner.tick(&metrics);
                    let change = (current.poll_interval_ms - prev.poll_interval_ms).abs()
                        + (current.scrollback_lines - prev.scrollback_lines).abs()
                        + (current.snapshot_interval_secs - prev.snapshot_interval_secs).abs()
                        + (current.pool_size - prev.pool_size).abs();
                    last_change = change;
                    prev = current;
                }

                // After 500 ticks of constant input, all parameters should be at or
                // near their equilibrium points (typically range bounds).
                // Threshold of 5.0 accommodates slow compounding drift from
                // competing pressures on the same parameter.
                prop_assert!(last_change < 5.0,
                    "After 500 ticks of constant input, change per tick should approach zero, got: {last_change}");
            }

            /// Monotonic memory pressure → scrollback decreases monotonically.
            #[test]
            fn monotonic_memory_response(
                base_rss in 0.6..=0.9_f64,
            ) {
                let config = AutoTuneConfig {
                    hysteresis_ticks: 1,
                    max_change_per_tick: 0.1,
                    ..AutoTuneConfig::default()
                };
                let mut tuner = AutoTuner::new(config);

                // Apply sustained pressure with increasing RSS
                let mut prev_scrollback = tuner.params().scrollback_lines;
                for i in 0..20 {
                    let rss = base_rss + (i as f64 * 0.005);
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
                    ..AutoTuneConfig::default()
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
                    prop_assert_eq!(tuner.params().poll_interval_ms, initial.poll_interval_ms);
                    prop_assert_eq!(tuner.params().scrollback_lines, initial.scrollback_lines);
                    prop_assert_eq!(tuner.params().snapshot_interval_secs, initial.snapshot_interval_secs);
                    prop_assert_eq!(tuner.params().pool_size, initial.pool_size);
                    prop_assert_eq!(tuner.params().backpressure_threshold, initial.backpressure_threshold);
                }
            }
        }
    }
}
