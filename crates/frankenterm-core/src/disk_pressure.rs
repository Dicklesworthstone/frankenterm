//! Disk pressure monitoring for adaptive storage-ballast decisions.
//!
//! Samples filesystem free space, smooths usage with EWMA, applies a PID
//! correction term, and classifies pressure into four tiers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};

// =============================================================================
// Pressure tiers
// =============================================================================

/// Disk pressure severity tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskPressureTier {
    /// Safe operating range.
    Green,
    /// Elevated pressure.
    Yellow,
    /// High pressure; reclaim actions should begin.
    Red,
    /// Critical pressure; emergency reclaim path.
    Black,
}

impl std::fmt::Display for DiskPressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "GREEN"),
            Self::Yellow => write!(f, "YELLOW"),
            Self::Red => write!(f, "RED"),
            Self::Black => write!(f, "BLACK"),
        }
    }
}

impl DiskPressureTier {
    /// Numeric representation (0-3) for metrics and atomic storage.
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

// =============================================================================
// Configuration
// =============================================================================

/// Usage-fraction thresholds for each elevated tier.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct PressureThresholds {
    /// Usage fraction at which Yellow begins.
    pub yellow: f64,
    /// Usage fraction at which Red begins.
    pub red: f64,
    /// Usage fraction at which Black begins.
    pub black: f64,
}

impl Default for PressureThresholds {
    fn default() -> Self {
        Self {
            yellow: 0.70,
            red: 0.85,
            black: 0.95,
        }
    }
}

impl PressureThresholds {
    #[must_use]
    fn normalized(self) -> Self {
        let yellow = self.yellow.clamp(0.0, 1.0);
        let red = self.red.clamp(yellow, 1.0);
        let black = self.black.clamp(red, 1.0);
        Self { yellow, red, black }
    }
}

/// Disk pressure monitoring configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskPressureConfig {
    /// Enable monitoring.
    pub enabled: bool,
    /// Sampling root path (filesystem containing this path is sampled).
    pub root_path: PathBuf,
    /// Suggested poll interval in milliseconds.
    pub poll_interval_ms: u64,
    /// EWMA alpha in [0.0, 1.0].
    pub ewma_alpha: f64,
    /// PID proportional gain.
    pub pid_kp: f64,
    /// PID integral gain.
    pub pid_ki: f64,
    /// PID derivative gain.
    pub pid_kd: f64,
    /// Lower clamp bound for integral anti-windup.
    pub pid_integral_min: f64,
    /// Upper clamp bound for integral anti-windup.
    pub pid_integral_max: f64,
    /// Target steady-state usage fraction (controller setpoint).
    pub target_usage_fraction: f64,
    /// Tier thresholds.
    pub thresholds: PressureThresholds,
}

impl Default for DiskPressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root_path: PathBuf::from("/"),
            poll_interval_ms: 5_000,
            ewma_alpha: 0.30,
            pid_kp: 0.60,
            pid_ki: 0.10,
            pid_kd: 0.05,
            pid_integral_min: -1.0,
            pid_integral_max: 1.0,
            target_usage_fraction: 0.75,
            thresholds: PressureThresholds::default(),
        }
    }
}

impl DiskPressureConfig {
    #[must_use]
    fn normalized(mut self) -> Self {
        self.ewma_alpha = self.ewma_alpha.clamp(0.0, 1.0);
        self.target_usage_fraction = self.target_usage_fraction.clamp(0.0, 1.0);

        if self.pid_integral_min > self.pid_integral_max {
            std::mem::swap(&mut self.pid_integral_min, &mut self.pid_integral_max);
        }

        self.thresholds = self.thresholds.normalized();
        self
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the disk pressure monitor.
///
/// All counters are plain `u64` because `DiskPressureMonitor` uses `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct DiskPressureTelemetry {
    /// Total update() / update_with_sample() calls.
    updates: u64,
    /// Updates skipped because monitoring is disabled.
    updates_disabled: u64,
    /// Tier classifications that resulted in Green.
    tier_green: u64,
    /// Tier classifications that resulted in Yellow.
    tier_yellow: u64,
    /// Tier classifications that resulted in Red.
    tier_red: u64,
    /// Tier classifications that resulted in Black.
    tier_black: u64,
    /// Number of tier transitions (tier changed from previous).
    tier_transitions: u64,
}

impl DiskPressureTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> DiskPressureTelemetrySnapshot {
        DiskPressureTelemetrySnapshot {
            updates: self.updates,
            updates_disabled: self.updates_disabled,
            tier_green: self.tier_green,
            tier_yellow: self.tier_yellow,
            tier_red: self.tier_red,
            tier_black: self.tier_black,
            tier_transitions: self.tier_transitions,
        }
    }
}

/// Serializable snapshot of disk pressure telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskPressureTelemetrySnapshot {
    /// Total update() / update_with_sample() calls.
    pub updates: u64,
    /// Updates skipped because monitoring is disabled.
    pub updates_disabled: u64,
    /// Tier classifications that resulted in Green.
    pub tier_green: u64,
    /// Tier classifications that resulted in Yellow.
    pub tier_yellow: u64,
    /// Tier classifications that resulted in Red.
    pub tier_red: u64,
    /// Tier classifications that resulted in Black.
    pub tier_black: u64,
    /// Number of tier transitions (tier changed from previous).
    pub tier_transitions: u64,
}

// =============================================================================
// Core sample + snapshot types
// =============================================================================

/// A single disk sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskSample {
    /// Available bytes on the sampled filesystem.
    pub available_bytes: u64,
    /// Total bytes on the sampled filesystem.
    pub total_bytes: u64,
    /// Fraction used in [0.0, 1.0].
    pub usage_fraction: f64,
    /// Timestamp of the sample.
    pub sampled_at: Instant,
}

/// Serializable monitor snapshot for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PressureSnapshot {
    /// Current classified tier.
    pub tier: DiskPressureTier,
    /// Snapshot timestamp (unix epoch ms).
    pub timestamp_epoch_ms: u64,
    /// Last observed available bytes.
    pub available_bytes: u64,
    /// Last observed total bytes.
    pub total_bytes: u64,
    /// Last raw usage fraction.
    pub usage_fraction: f64,
    /// Last EWMA-smoothed usage fraction.
    pub smoothed_usage_fraction: f64,
    /// Last controller error (smoothed - target).
    pub pid_error: f64,
    /// Current integral accumulator.
    pub pid_integral: f64,
    /// Last derivative term.
    pub pid_derivative: f64,
    /// Last full PID output term.
    pub pid_output: f64,
    /// Effective usage used for tier classification.
    pub effective_usage_fraction: f64,
    /// Number of completed update cycles.
    pub update_count: u64,
}

// =============================================================================
// EWMA
// =============================================================================

/// Exponentially weighted moving-average estimator.
#[derive(Debug, Clone)]
pub struct EwmaEstimator {
    alpha: f64,
    value: f64,
    initialized: bool,
}

impl EwmaEstimator {
    /// Create a new estimator with the provided alpha in [0.0, 1.0].
    #[must_use]
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha: alpha.clamp(0.0, 1.0),
            value: 0.0,
            initialized: false,
        }
    }

    /// Feed a new sample and return the smoothed value.
    pub fn update(&mut self, sample: f64) -> f64 {
        let sample = sample.clamp(0.0, 1.0);
        if !self.initialized {
            self.value = sample;
            self.initialized = true;
        } else {
            self.value = self.alpha.mul_add(sample, (1.0 - self.alpha) * self.value);
        }
        self.value
    }

    /// Return the current smoothed value (0.0 before first sample).
    #[must_use]
    pub fn current(&self) -> f64 {
        self.value
    }
}

// =============================================================================
// PID controller
// =============================================================================

/// PID controller with integral anti-windup bounds.
#[derive(Debug, Clone)]
pub struct PidController {
    kp: f64,
    ki: f64,
    kd: f64,
    integral_min: f64,
    integral_max: f64,
    integral: f64,
    previous_error: Option<f64>,
    last_derivative: f64,
}

impl PidController {
    /// Create a controller.
    #[must_use]
    pub fn new(kp: f64, ki: f64, kd: f64, integral_min: f64, integral_max: f64) -> Self {
        let (integral_min, integral_max) = if integral_min <= integral_max {
            (integral_min, integral_max)
        } else {
            (integral_max, integral_min)
        };

        Self {
            kp,
            ki,
            kd,
            integral_min,
            integral_max,
            integral: 0.0,
            previous_error: None,
            last_derivative: 0.0,
        }
    }

    /// Update the controller state and return the control signal.
    pub fn update(&mut self, error: f64, dt_secs: f64) -> f64 {
        let dt_secs = dt_secs.max(f64::EPSILON);

        self.integral = error
            .mul_add(dt_secs, self.integral)
            .clamp(self.integral_min, self.integral_max);

        self.last_derivative = self
            .previous_error
            .map_or(0.0, |prev| (error - prev) / dt_secs);
        self.previous_error = Some(error);

        self.kp.mul_add(
            error,
            self.ki
                .mul_add(self.integral, self.kd * self.last_derivative),
        )
    }

    /// Reset controller state.
    pub fn reset(&mut self) {
        self.integral = 0.0;
        self.previous_error = None;
        self.last_derivative = 0.0;
    }

    /// Current integral value.
    #[must_use]
    pub fn integral(&self) -> f64 {
        self.integral
    }

    /// Last derivative value.
    #[must_use]
    pub fn derivative(&self) -> f64 {
        self.last_derivative
    }
}

// =============================================================================
// Monitor
// =============================================================================

/// Disk pressure monitor with EWMA + PID shaping and lock-free tier reads.
pub struct DiskPressureMonitor {
    config: DiskPressureConfig,
    latest_tier: Arc<AtomicU64>,
    ewma: EwmaEstimator,
    pid: PidController,
    last_sample: Option<DiskSample>,
    last_smoothed_usage: f64,
    last_pid_error: f64,
    last_pid_output: f64,
    last_effective_usage: f64,
    update_count: u64,
    /// Operational telemetry counters.
    telemetry: DiskPressureTelemetry,
}

impl DiskPressureMonitor {
    /// Create a new monitor.
    #[must_use]
    pub fn new(config: DiskPressureConfig) -> Self {
        let config = config.normalized();
        let ewma = EwmaEstimator::new(config.ewma_alpha);
        let pid = PidController::new(
            config.pid_kp,
            config.pid_ki,
            config.pid_kd,
            config.pid_integral_min,
            config.pid_integral_max,
        );

        Self {
            config,
            latest_tier: Arc::new(AtomicU64::new(0)),
            ewma,
            pid,
            last_sample: None,
            last_smoothed_usage: 0.0,
            last_pid_error: 0.0,
            last_pid_output: 0.0,
            last_effective_usage: 0.0,
            update_count: 0,
            telemetry: DiskPressureTelemetry::new(),
        }
    }

    /// Current tier via lock-free atomic read.
    #[must_use]
    pub fn current_tier(&self) -> DiskPressureTier {
        match self.latest_tier.load(Ordering::Relaxed) {
            1 => DiskPressureTier::Yellow,
            2 => DiskPressureTier::Red,
            3 => DiskPressureTier::Black,
            _ => DiskPressureTier::Green,
        }
    }

    /// Shared atomic handle for other tasks.
    #[must_use]
    pub fn tier_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.latest_tier)
    }

    /// Take a disk-space sample.
    #[must_use]
    pub fn sample(&self) -> DiskSample {
        let (available_bytes, total_bytes) = read_disk_space_statvfs(&self.config.root_path)
            .or_else(|| read_disk_space_df(&self.config.root_path))
            .unwrap_or((0, 0));

        let usage_fraction = if total_bytes == 0 {
            0.0
        } else {
            let available = available_bytes.min(total_bytes);
            (1.0 - (available as f64 / total_bytes as f64)).clamp(0.0, 1.0)
        };

        DiskSample {
            available_bytes,
            total_bytes,
            usage_fraction,
            sampled_at: Instant::now(),
        }
    }

    /// Sample + smooth + control + classify; returns the new tier.
    pub fn update(&mut self) -> DiskPressureTier {
        if !self.config.enabled {
            self.telemetry.updates_disabled += 1;
            return self.current_tier();
        }

        let sample = self.sample();
        self.update_inner(sample)
    }

    /// Update with a synthetic sample (for testing without real disk I/O).
    pub fn update_with_sample(&mut self, sample: DiskSample) -> DiskPressureTier {
        if !self.config.enabled {
            self.telemetry.updates_disabled += 1;
            return self.current_tier();
        }
        self.update_inner(sample)
    }

    /// Shared update logic used by both update() and update_with_sample().
    fn update_inner(&mut self, sample: DiskSample) -> DiskPressureTier {
        self.telemetry.updates += 1;
        let prev_tier = self.current_tier();

        let smoothed_usage = self.ewma.update(sample.usage_fraction);

        let dt_secs = self.last_sample.map_or_else(
            || (self.config.poll_interval_ms.max(1) as f64) / 1000.0,
            |previous| {
                sample
                    .sampled_at
                    .saturating_duration_since(previous.sampled_at)
                    .as_secs_f64()
                    .max(f64::EPSILON)
            },
        );

        let pid_error = smoothed_usage - self.config.target_usage_fraction;
        let pid_output = self.pid.update(pid_error, dt_secs);
        let effective_usage = (smoothed_usage + pid_output).clamp(0.0, 1.0);
        let tier = classify_tier(effective_usage, self.config.thresholds);

        match tier {
            DiskPressureTier::Green => self.telemetry.tier_green += 1,
            DiskPressureTier::Yellow => self.telemetry.tier_yellow += 1,
            DiskPressureTier::Red => self.telemetry.tier_red += 1,
            DiskPressureTier::Black => self.telemetry.tier_black += 1,
        }
        if tier != prev_tier {
            self.telemetry.tier_transitions += 1;
        }

        self.latest_tier
            .store(tier.as_u8() as u64, Ordering::Relaxed);
        self.last_sample = Some(sample);
        self.last_smoothed_usage = smoothed_usage;
        self.last_pid_error = pid_error;
        self.last_pid_output = pid_output;
        self.last_effective_usage = effective_usage;
        self.update_count += 1;

        tier
    }

    /// Diagnostic snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self) -> PressureSnapshot {
        let (available_bytes, total_bytes, usage_fraction) =
            self.last_sample.map_or((0, 0, 0.0), |sample| {
                (
                    sample.available_bytes,
                    sample.total_bytes,
                    sample.usage_fraction,
                )
            });

        PressureSnapshot {
            tier: self.current_tier(),
            timestamp_epoch_ms: now_epoch_ms(),
            available_bytes,
            total_bytes,
            usage_fraction,
            smoothed_usage_fraction: self.last_smoothed_usage,
            pid_error: self.last_pid_error,
            pid_integral: self.pid.integral(),
            pid_derivative: self.pid.derivative(),
            pid_output: self.last_pid_output,
            effective_usage_fraction: self.last_effective_usage,
            update_count: self.update_count,
        }
    }

    /// Access the operational telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &DiskPressureTelemetry {
        &self.telemetry
    }
}

// =============================================================================
// Internal helpers
// =============================================================================

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn classify_tier(usage_fraction: f64, thresholds: PressureThresholds) -> DiskPressureTier {
    let usage_fraction = usage_fraction.clamp(0.0, 1.0);
    let thresholds = thresholds.normalized();

    if usage_fraction >= thresholds.black {
        DiskPressureTier::Black
    } else if usage_fraction >= thresholds.red {
        DiskPressureTier::Red
    } else if usage_fraction >= thresholds.yellow {
        DiskPressureTier::Yellow
    } else {
        DiskPressureTier::Green
    }
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)] // u64::from() is intentional for 32-bit platform compat
fn read_disk_space_statvfs(path: &Path) -> Option<(u64, u64)> {
    let vfs = statvfs(path).ok()?;
    let block_size = u64::from(vfs.fragment_size().max(1));
    let total_blocks = u64::from(vfs.blocks());
    let available_blocks = u64::from(vfs.blocks_available());
    let total_bytes = total_blocks.saturating_mul(block_size);
    let available_bytes = available_blocks.saturating_mul(block_size);
    Some((available_bytes.min(total_bytes), total_bytes))
}

#[cfg(not(unix))]
fn read_disk_space_statvfs(path: &Path) -> Option<(u64, u64)> {
    let _ = path;
    None
}

fn read_disk_space_df(path: &Path) -> Option<(u64, u64)> {
    let path_str = path.to_str()?;
    let output = Command::new("df").args(["-k", path_str]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_df_output_kib(&stdout)
}

fn parse_df_output_kib(output: &str) -> Option<(u64, u64)> {
    output
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .filter_map(parse_df_data_line)
        .last()
}

fn parse_df_data_line(line: &str) -> Option<(u64, u64)> {
    let cols: Vec<&str> = line.split_whitespace().collect();
    let usage_idx = cols.iter().position(|col| col.ends_with('%'))?;

    if usage_idx < 3 {
        return None;
    }

    let total_kib = cols.get(usage_idx - 3)?.parse::<u64>().ok()?;
    let available_kib = cols.get(usage_idx - 1)?.parse::<u64>().ok()?;
    Some((
        available_kib.saturating_mul(1024),
        total_kib.saturating_mul(1024),
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        DiskPressureConfig, DiskPressureMonitor, DiskPressureTier, EwmaEstimator, PidController,
        PressureThresholds, classify_tier, parse_df_data_line, parse_df_output_kib,
    };

    #[test]
    fn tier_ordering_and_labels_are_stable() {
        assert!(DiskPressureTier::Green < DiskPressureTier::Yellow);
        assert!(DiskPressureTier::Yellow < DiskPressureTier::Red);
        assert!(DiskPressureTier::Red < DiskPressureTier::Black);

        assert_eq!(DiskPressureTier::Green.as_u8(), 0);
        assert_eq!(DiskPressureTier::Yellow.as_u8(), 1);
        assert_eq!(DiskPressureTier::Red.as_u8(), 2);
        assert_eq!(DiskPressureTier::Black.as_u8(), 3);

        assert_eq!(DiskPressureTier::Black.to_string(), "BLACK");
    }

    #[test]
    fn ewma_tracks_samples() {
        let mut ewma = EwmaEstimator::new(0.5);
        assert!(ewma.current().abs() < f64::EPSILON);
        assert!((ewma.update(0.2) - 0.2).abs() < f64::EPSILON);
        assert!((ewma.update(0.6) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn pid_clamps_integral_term() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, -0.25, 0.25);
        for _ in 0..20 {
            let _ = pid.update(1.0, 1.0);
        }
        assert!((pid.integral() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn pid_reset_clears_state() {
        let mut pid = PidController::new(1.0, 1.0, 1.0, -1.0, 1.0);
        let _ = pid.update(0.5, 1.0);
        pid.reset();
        assert!(pid.integral().abs() < f64::EPSILON);
        assert!(pid.derivative().abs() < f64::EPSILON);
    }

    #[test]
    fn classify_uses_normalized_thresholds() {
        let thresholds = PressureThresholds {
            yellow: 0.9,
            red: 0.5,
            black: 0.7,
        };
        assert_eq!(classify_tier(0.2, thresholds), DiskPressureTier::Green);
        assert_eq!(classify_tier(0.9, thresholds), DiskPressureTier::Black);
    }

    #[test]
    fn parse_df_line_linux_style() {
        let line = "/dev/disk3s1 100000 70000 30000 70% /";
        let parsed = parse_df_data_line(line).expect("expected parse success");
        assert_eq!(parsed.1, 100_000 * 1024);
        assert_eq!(parsed.0, 30_000 * 1024);
    }

    #[test]
    fn parse_df_line_macos_style_with_inode_columns() {
        let line = "/dev/disk3s1 245113536 194608736 49714792 80% 853965 49602192 2% /";
        let parsed = parse_df_data_line(line).expect("expected parse success");
        assert_eq!(parsed.1, 245_113_536 * 1024);
        assert_eq!(parsed.0, 49_714_792 * 1024);
    }

    #[test]
    fn monitor_update_publishes_tier_and_snapshot() {
        let mut monitor = DiskPressureMonitor::new(DiskPressureConfig {
            target_usage_fraction: 0.0,
            thresholds: PressureThresholds {
                yellow: 0.0,
                red: 0.0,
                black: 0.0,
            },
            ..DiskPressureConfig::default()
        });

        let tier = monitor.update();
        assert_eq!(tier, monitor.current_tier());

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.tier, tier);
        assert_eq!(snapshot.update_count, 1);
    }

    #[test]
    fn ewma_alpha_zero_holds_first_sample() {
        let mut ewma = EwmaEstimator::new(0.0);
        assert!((ewma.update(0.3) - 0.3).abs() < 1e-9);
        assert!((ewma.update(0.9) - 0.3).abs() < 1e-9);
    }

    #[test]
    fn ewma_alpha_one_tracks_latest_sample() {
        let mut ewma = EwmaEstimator::new(1.0);
        assert!((ewma.update(0.1) - 0.1).abs() < 1e-9);
        assert!((ewma.update(0.9) - 0.9).abs() < 1e-9);
    }

    #[test]
    fn ewma_clamps_samples_to_unit_interval() {
        let mut ewma = EwmaEstimator::new(0.5);
        assert!((ewma.update(-5.0) - 0.0).abs() < 1e-9);
        assert!((ewma.update(5.0) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn ewma_is_monotone_for_monotone_input() {
        let mut ewma = EwmaEstimator::new(0.4);
        let mut prev = ewma.update(0.1);
        for sample in [0.2, 0.3, 0.4, 0.8, 1.0] {
            let current = ewma.update(sample);
            assert!(current >= prev);
            prev = current;
        }
    }

    #[test]
    fn ewma_converges_to_constant_signal() {
        let mut ewma = EwmaEstimator::new(0.2);
        let _ = ewma.update(0.0);
        for _ in 0..60 {
            let _ = ewma.update(1.0);
        }
        assert!(ewma.current() > 0.99);
    }

    #[test]
    fn pid_zero_error_has_zero_output() {
        let mut pid = PidController::new(1.0, 1.0, 1.0, -1.0, 1.0);
        let output = pid.update(0.0, 1.0);
        assert!(output.abs() < 1e-9);
    }

    #[test]
    fn pid_proportional_only_matches_kp_times_error() {
        let mut pid = PidController::new(2.0, 0.0, 0.0, -1.0, 1.0);
        let output = pid.update(0.25, 1.0);
        assert!((output - 0.5).abs() < 1e-9);
    }

    #[test]
    fn pid_integral_accumulates_over_dt() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, -10.0, 10.0);
        let output = pid.update(0.5, 2.0);
        assert!((pid.integral() - 1.0).abs() < 1e-9);
        assert!((output - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pid_derivative_responds_to_error_change() {
        let mut pid = PidController::new(0.0, 0.0, 1.0, -10.0, 10.0);
        let _ = pid.update(0.0, 1.0);
        let output = pid.update(1.0, 0.5);
        assert!((output - 2.0).abs() < 1e-9);
    }

    #[test]
    fn pid_first_update_derivative_is_zero() {
        let mut pid = PidController::new(0.0, 0.0, 1.0, -10.0, 10.0);
        let output = pid.update(10.0, 1.0);
        assert!(output.abs() < 1e-9);
        assert!(pid.derivative().abs() < 1e-9);
    }

    #[test]
    fn pid_lower_integral_clamp_is_enforced() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, -0.2, 0.2);
        for _ in 0..20 {
            let _ = pid.update(-1.0, 1.0);
        }
        assert!((pid.integral() + 0.2).abs() < 1e-9);
    }

    #[test]
    fn pid_swaps_inverted_integral_bounds() {
        let mut pid = PidController::new(0.0, 1.0, 0.0, 2.0, -2.0);
        for _ in 0..20 {
            let _ = pid.update(1.0, 1.0);
        }
        assert!(pid.integral() <= 2.0);
        assert!(pid.integral() >= -2.0);
    }

    #[test]
    fn pid_zero_dt_is_still_finite() {
        let mut pid = PidController::new(0.1, 0.1, 0.1, -1.0, 1.0);
        let _ = pid.update(0.0, 1.0);
        let output = pid.update(1.0, 0.0);
        assert!(output.is_finite());
    }

    #[test]
    fn classify_boundaries_are_inclusive() {
        let thresholds = PressureThresholds::default();
        assert_eq!(
            classify_tier(thresholds.yellow, thresholds),
            DiskPressureTier::Yellow
        );
        assert_eq!(
            classify_tier(thresholds.red, thresholds),
            DiskPressureTier::Red
        );
        assert_eq!(
            classify_tier(thresholds.black, thresholds),
            DiskPressureTier::Black
        );
    }

    #[test]
    fn classify_below_yellow_is_green() {
        let thresholds = PressureThresholds::default();
        assert_eq!(
            classify_tier(thresholds.yellow - 0.0001, thresholds),
            DiskPressureTier::Green
        );
    }

    #[test]
    fn classify_clamps_extreme_usage_values() {
        let thresholds = PressureThresholds::default();
        assert_eq!(classify_tier(-100.0, thresholds), DiskPressureTier::Green);
        assert_eq!(classify_tier(100.0, thresholds), DiskPressureTier::Black);
    }

    #[test]
    fn parse_df_line_rejects_missing_percent_column() {
        let line = "/dev/disk3s1 100000 70000 30000 mounted_at_root";
        assert!(parse_df_data_line(line).is_none());
    }

    #[test]
    fn parse_df_line_rejects_too_few_columns() {
        let line = "/dev/disk3s1 100000 70%";
        assert!(parse_df_data_line(line).is_none());
    }

    #[test]
    fn parse_df_output_uses_last_valid_line() {
        let output = "Filesystem 512-blocks Used Available Capacity Mounted on\n\
/dev/disk0s2 100 40 60 40% /\n\
/dev/disk0s3 200 180 20 90% /data\n";
        let parsed = parse_df_output_kib(output).expect("expected parse success");
        assert_eq!(parsed.1, 200 * 1024);
        assert_eq!(parsed.0, 20 * 1024);
    }

    #[test]
    fn parse_df_output_ignores_blank_lines() {
        let output = "Filesystem 512-blocks Used Available Capacity Mounted on\n\n\
/dev/disk0s2 100 40 60 40% /\n\n";
        let parsed = parse_df_output_kib(output).expect("expected parse success");
        assert_eq!(parsed.1, 100 * 1024);
        assert_eq!(parsed.0, 60 * 1024);
    }

    #[test]
    fn parse_df_output_without_data_returns_none() {
        let output = "Filesystem 512-blocks Used Available Capacity Mounted on\n";
        assert!(parse_df_output_kib(output).is_none());
    }

    #[test]
    fn monitor_initial_state_is_green_with_zero_updates() {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        assert_eq!(monitor.current_tier(), DiskPressureTier::Green);
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.tier, DiskPressureTier::Green);
        assert_eq!(snapshot.update_count, 0);
    }

    #[test]
    fn monitor_tier_handle_matches_current_tier() {
        let mut monitor = DiskPressureMonitor::new(DiskPressureConfig {
            target_usage_fraction: 0.0,
            thresholds: PressureThresholds {
                yellow: 0.0,
                red: 0.0,
                black: 0.0,
            },
            ..DiskPressureConfig::default()
        });
        let tier_handle = monitor.tier_handle();
        let tier = monitor.update();
        assert_eq!(
            tier_handle.load(std::sync::atomic::Ordering::Relaxed),
            tier.as_u8() as u64
        );
    }

    #[test]
    fn monitor_disabled_does_not_advance_state() {
        let mut monitor = DiskPressureMonitor::new(DiskPressureConfig {
            enabled: false,
            ..DiskPressureConfig::default()
        });
        let tier = monitor.update();
        assert_eq!(tier, DiskPressureTier::Green);
        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.update_count, 0);
    }

    #[test]
    fn monitor_sample_returns_consistent_ranges() {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        let sample = monitor.sample();
        assert!(sample.usage_fraction.is_finite());
        assert!(sample.usage_fraction >= 0.0);
        assert!(sample.usage_fraction <= 1.0);
        assert!(sample.total_bytes >= sample.available_bytes);
    }

    #[test]
    fn monitor_update_count_increments_per_call() {
        let mut monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        let _ = monitor.update();
        let _ = monitor.update();
        assert_eq!(monitor.snapshot().update_count, 2);
    }

    #[test]
    fn monitor_snapshot_numeric_fields_are_finite() {
        let mut monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        let _ = monitor.update();
        let snapshot = monitor.snapshot();
        assert!(snapshot.usage_fraction.is_finite());
        assert!(snapshot.smoothed_usage_fraction.is_finite());
        assert!(snapshot.pid_error.is_finite());
        assert!(snapshot.pid_output.is_finite());
        assert!(snapshot.effective_usage_fraction.is_finite());
    }

    #[test]
    fn monitor_snapshot_timestamp_is_nonzero() {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        assert!(monitor.snapshot().timestamp_epoch_ms > 0);
    }

    #[test]
    fn tier_serde_roundtrip() {
        let tier = DiskPressureTier::Red;
        let json = serde_json::to_string(&tier).expect("serialize tier");
        let parsed: DiskPressureTier = serde_json::from_str(&json).expect("deserialize tier");
        assert_eq!(parsed, DiskPressureTier::Red);
    }

    #[test]
    fn tier_display_strings_match_expected() {
        assert_eq!(DiskPressureTier::Green.to_string(), "GREEN");
        assert_eq!(DiskPressureTier::Yellow.to_string(), "YELLOW");
        assert_eq!(DiskPressureTier::Red.to_string(), "RED");
        assert_eq!(DiskPressureTier::Black.to_string(), "BLACK");
    }
}
