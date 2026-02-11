//! CPU pressure monitoring for adaptive scheduling.
//!
//! Samples system CPU utilization and classifies it into pressure tiers
//! that the runtime uses to throttle capture frequency and shed load.
//!
//! - **Linux**: reads `/proc/pressure/cpu` (PSI avg10) for accurate
//!   pressure stall information.
//! - **macOS**: reads 1-minute load average via `sysctl` and normalizes
//!   by CPU count (no PSI equivalent, no unsafe FFI required).
//! - **Other**: returns `Green` (no monitoring available).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// =============================================================================
// Pressure tiers
// =============================================================================

/// CPU pressure severity tier.
///
/// Aligned with [`BackpressureTier`](crate::backpressure::BackpressureTier) for
/// composable decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CpuPressureTier {
    /// CPU utilization below warning threshold.
    Green,
    /// Moderate pressure — reduce non-essential work.
    Yellow,
    /// High pressure — pause idle panes, reduce capture frequency.
    Orange,
    /// Critical — emergency throttling, kill stuck processes.
    Red,
}

impl std::fmt::Display for CpuPressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "GREEN"),
            Self::Yellow => write!(f, "YELLOW"),
            Self::Orange => write!(f, "ORANGE"),
            Self::Red => write!(f, "RED"),
        }
    }
}

impl CpuPressureTier {
    /// Numeric value for gauge metrics (0-3).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Yellow => 1,
            Self::Orange => 2,
            Self::Red => 3,
        }
    }

    /// Suggested capture interval multiplier for this tier.
    #[must_use]
    pub const fn capture_interval_multiplier(self) -> u32 {
        match self {
            Self::Green => 1,
            Self::Yellow => 2,
            Self::Orange => 4,
            Self::Red => 8,
        }
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// CPU pressure monitoring configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CpuPressureConfig {
    /// Enable CPU pressure monitoring.
    pub enabled: bool,
    /// Sample interval in milliseconds.
    pub sample_interval_ms: u64,
    /// PSI avg10 threshold for Yellow (Linux) or CPU% for macOS.
    pub yellow_threshold: f64,
    /// PSI avg10 threshold for Orange.
    pub orange_threshold: f64,
    /// PSI avg10 threshold for Red.
    pub red_threshold: f64,
}

impl Default for CpuPressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 5000,
            yellow_threshold: 15.0,
            orange_threshold: 30.0,
            red_threshold: 50.0,
        }
    }
}

// =============================================================================
// CPU sample
// =============================================================================

/// A single CPU pressure sample.
#[derive(Debug, Clone)]
pub struct CpuSample {
    /// Raw pressure value (PSI avg10 on Linux, load-avg% on macOS).
    pub pressure: f64,
    /// Classified tier based on thresholds.
    pub tier: CpuPressureTier,
    /// Timestamp of the sample.
    pub sampled_at: Instant,
}

// =============================================================================
// Monitor
// =============================================================================

/// CPU pressure monitor that samples system CPU utilization.
///
/// Thread-safe. Uses atomic operations for the latest tier.
pub struct CpuPressureMonitor {
    config: CpuPressureConfig,
    /// Latest tier as atomic u8 (0=Green, 1=Yellow, 2=Orange, 3=Red).
    latest_tier: Arc<AtomicU64>,
    /// Number of logical CPUs (cached at construction).
    ncpu: u32,
}

impl CpuPressureMonitor {
    /// Create a new monitor with the given configuration.
    pub fn new(config: CpuPressureConfig) -> Self {
        Self {
            config,
            latest_tier: Arc::new(AtomicU64::new(0)),
            ncpu: detect_ncpu(),
        }
    }

    /// Get the latest pressure tier (lock-free read).
    #[must_use]
    pub fn current_tier(&self) -> CpuPressureTier {
        match self.latest_tier.load(Ordering::Relaxed) {
            1 => CpuPressureTier::Yellow,
            2 => CpuPressureTier::Orange,
            3 => CpuPressureTier::Red,
            _ => CpuPressureTier::Green,
        }
    }

    /// Get an Arc to the tier atomic for sharing with other tasks.
    #[must_use]
    pub fn tier_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.latest_tier)
    }

    /// Take a single CPU pressure sample.
    pub fn sample(&self) -> CpuSample {
        let pressure = self.read_cpu_pressure();
        let tier = self.classify(pressure);
        self.latest_tier
            .store(tier.as_u8() as u64, Ordering::Relaxed);
        CpuSample {
            pressure,
            tier,
            sampled_at: Instant::now(),
        }
    }

    /// Run the monitoring loop until the shutdown flag is set.
    pub async fn run(&self, shutdown: Arc<std::sync::atomic::AtomicBool>) {
        let interval = Duration::from_millis(self.config.sample_interval_ms.max(1000));
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;
            if shutdown.load(Ordering::SeqCst) {
                break;
            }

            let sample = self.sample();
            if sample.tier >= CpuPressureTier::Yellow {
                tracing::info!(
                    pressure = sample.pressure,
                    tier = %sample.tier,
                    "CPU pressure elevated"
                );
            }
        }
    }

    /// Classify a raw pressure value into a tier.
    fn classify(&self, pressure: f64) -> CpuPressureTier {
        if pressure >= self.config.red_threshold {
            CpuPressureTier::Red
        } else if pressure >= self.config.orange_threshold {
            CpuPressureTier::Orange
        } else if pressure >= self.config.yellow_threshold {
            CpuPressureTier::Yellow
        } else {
            CpuPressureTier::Green
        }
    }

    // -------------------------------------------------------------------------
    // Platform-specific sampling
    // -------------------------------------------------------------------------

    /// Read raw CPU pressure metric from the OS.
    ///
    /// - **Linux**: PSI avg10 from `/proc/pressure/cpu`
    /// - **macOS**: 1-min load average normalized by CPU count (× 100)
    /// - **Other**: always 0.0
    fn read_cpu_pressure(&self) -> f64 {
        #[cfg(target_os = "linux")]
        {
            read_linux_psi_avg10()
        }
        #[cfg(target_os = "macos")]
        {
            let load = read_macos_load_avg();
            // Normalize: load_avg / ncpu * 100 → percentage-like metric
            (load / self.ncpu.max(1) as f64) * 100.0
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = self.ncpu; // suppress unused warning
            0.0
        }
    }
}

// =============================================================================
// Linux: /proc/pressure/cpu PSI
// =============================================================================

/// Read the PSI avg10 value from `/proc/pressure/cpu`.
///
/// Format: `some avg10=X.XX avg60=X.XX avg300=X.XX total=XXXX`
#[cfg(target_os = "linux")]
fn read_linux_psi_avg10() -> f64 {
    let Ok(contents) = std::fs::read_to_string("/proc/pressure/cpu") else {
        return 0.0;
    };

    // Parse "some avg10=X.XX ..."
    for line in contents.lines() {
        if line.starts_with("some") {
            for part in line.split_whitespace() {
                if let Some(val) = part.strip_prefix("avg10=") {
                    return val.parse::<f64>().unwrap_or(0.0);
                }
            }
        }
    }

    0.0
}

// =============================================================================
// macOS: load average via sysctl (safe, no FFI)
// =============================================================================

/// Read the 1-minute load average from `sysctl -n vm.loadavg` on macOS.
///
/// Output format: `{ 2.49 2.15 2.12 }`
/// We parse the first (1-minute) value and normalize by CPU count to get
/// a percentage-like pressure metric comparable to Linux PSI.
#[cfg(target_os = "macos")]
fn read_macos_load_avg() -> f64 {
    let output = std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok();

    output
        .and_then(|o| {
            let s = String::from_utf8(o.stdout).ok()?;
            let trimmed = s
                .trim()
                .trim_start_matches('{')
                .trim_end_matches('}')
                .trim();
            trimmed.split_whitespace().next()?.parse::<f64>().ok()
        })
        .unwrap_or(0.0)
}

// =============================================================================
// CPU count detection
// =============================================================================

/// Detect the number of logical CPUs, cached once at monitor creation.
fn detect_ncpu() -> u32 {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.ncpu"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(1)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u32)
            .unwrap_or(1)
            .max(1)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        1
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CpuPressureConfig {
        CpuPressureConfig {
            enabled: true,
            sample_interval_ms: 1000,
            yellow_threshold: 15.0,
            orange_threshold: 30.0,
            red_threshold: 50.0,
        }
    }

    #[test]
    fn tier_ordering() {
        assert!(CpuPressureTier::Green < CpuPressureTier::Yellow);
        assert!(CpuPressureTier::Yellow < CpuPressureTier::Orange);
        assert!(CpuPressureTier::Orange < CpuPressureTier::Red);
    }

    #[test]
    fn tier_display() {
        assert_eq!(format!("{}", CpuPressureTier::Green), "GREEN");
        assert_eq!(format!("{}", CpuPressureTier::Red), "RED");
    }

    #[test]
    fn tier_numeric() {
        assert_eq!(CpuPressureTier::Green.as_u8(), 0);
        assert_eq!(CpuPressureTier::Yellow.as_u8(), 1);
        assert_eq!(CpuPressureTier::Orange.as_u8(), 2);
        assert_eq!(CpuPressureTier::Red.as_u8(), 3);
    }

    #[test]
    fn capture_interval_multiplier() {
        assert_eq!(CpuPressureTier::Green.capture_interval_multiplier(), 1);
        assert_eq!(CpuPressureTier::Yellow.capture_interval_multiplier(), 2);
        assert_eq!(CpuPressureTier::Orange.capture_interval_multiplier(), 4);
        assert_eq!(CpuPressureTier::Red.capture_interval_multiplier(), 8);
    }

    #[test]
    fn classify_green() {
        let monitor = CpuPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(0.0), CpuPressureTier::Green);
        assert_eq!(monitor.classify(14.9), CpuPressureTier::Green);
    }

    #[test]
    fn classify_yellow() {
        let monitor = CpuPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(15.0), CpuPressureTier::Yellow);
        assert_eq!(monitor.classify(29.9), CpuPressureTier::Yellow);
    }

    #[test]
    fn classify_orange() {
        let monitor = CpuPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(30.0), CpuPressureTier::Orange);
        assert_eq!(monitor.classify(49.9), CpuPressureTier::Orange);
    }

    #[test]
    fn classify_red() {
        let monitor = CpuPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(50.0), CpuPressureTier::Red);
        assert_eq!(monitor.classify(100.0), CpuPressureTier::Red);
    }

    #[test]
    fn current_tier_default_is_green() {
        let monitor = CpuPressureMonitor::new(test_config());
        assert_eq!(monitor.current_tier(), CpuPressureTier::Green);
    }

    #[test]
    fn sample_updates_tier() {
        let monitor = CpuPressureMonitor::new(test_config());
        let sample = monitor.sample();
        // Load-average based pressure can exceed 100% on busy systems
        assert!(sample.pressure >= 0.0);
        assert_eq!(sample.tier, monitor.current_tier());
    }

    #[test]
    fn tier_handle_shares_state() {
        let monitor = CpuPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();

        // Initially green
        assert_eq!(handle.load(Ordering::Relaxed), 0);

        // Manually set to Red
        handle.store(3, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), CpuPressureTier::Red);
    }

    #[test]
    fn default_config_values() {
        let cfg = CpuPressureConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.sample_interval_ms, 5000);
        assert!((cfg.yellow_threshold - 15.0).abs() < f64::EPSILON);
        assert!((cfg.orange_threshold - 30.0).abs() < f64::EPSILON);
        assert!((cfg.red_threshold - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = CpuPressureConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: CpuPressureConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.yellow_threshold - cfg.yellow_threshold).abs() < f64::EPSILON);
        assert!((parsed.red_threshold - cfg.red_threshold).abs() < f64::EPSILON);
    }

    #[test]
    fn tier_serde_roundtrip() {
        let tier = CpuPressureTier::Orange;
        let json = serde_json::to_string(&tier).unwrap();
        assert_eq!(json, "\"orange\"");
        let parsed: CpuPressureTier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tier);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_psi_parse() {
        // If /proc/pressure/cpu exists, we should get a valid value
        let val = read_linux_psi_avg10();
        assert!(val >= 0.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_load_avg_readable() {
        let load = read_macos_load_avg();
        assert!(load >= 0.0, "load average should be non-negative");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_cpu_pressure_normalized() {
        let monitor = CpuPressureMonitor::new(test_config());
        let pressure = monitor.read_cpu_pressure();
        // Normalized load average should be non-negative
        assert!(pressure >= 0.0);
    }

    #[test]
    fn detect_ncpu_returns_positive() {
        let n = detect_ncpu();
        assert!(n >= 1, "should detect at least 1 CPU");
    }
}
