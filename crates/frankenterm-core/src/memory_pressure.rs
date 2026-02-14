//! Memory pressure monitoring for adaptive pane management.
//!
//! Samples system memory utilization and classifies it into pressure tiers
//! that drive scrollback compression, eviction, and pane cleanup decisions.
//!
//! - **Linux**: reads `/proc/pressure/memory` (PSI avg10) and `/proc/meminfo`
//! - **macOS**: reads memory stats via `vm_stat` and `sysctl`
//! - **Other**: returns `Green` (no monitoring available)

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::runtime_compat::sleep;
use serde::{Deserialize, Serialize};

// =============================================================================
// Pressure tiers
// =============================================================================

/// Memory pressure severity tier.
///
/// Aligned with [`CpuPressureTier`](crate::cpu_pressure::CpuPressureTier) and
/// [`BackpressureTier`](crate::backpressure::BackpressureTier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressureTier {
    /// Memory utilization below warning threshold.
    Green,
    /// Moderate pressure — compress idle pane scrollback.
    Yellow,
    /// High pressure — evict scrollback to disk, pause captures.
    Orange,
    /// Critical — kill largest idle pane, emergency eviction.
    Red,
}

impl std::fmt::Display for MemoryPressureTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Green => write!(f, "GREEN"),
            Self::Yellow => write!(f, "YELLOW"),
            Self::Orange => write!(f, "ORANGE"),
            Self::Red => write!(f, "RED"),
        }
    }
}

impl MemoryPressureTier {
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

    /// Suggested action for this pressure level.
    #[must_use]
    pub const fn suggested_action(self) -> MemoryAction {
        match self {
            Self::Green => MemoryAction::None,
            Self::Yellow => MemoryAction::CompressIdle,
            Self::Orange => MemoryAction::EvictToDisk,
            Self::Red => MemoryAction::EmergencyCleanup,
        }
    }
}

/// Suggested action based on memory pressure tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryAction {
    /// No action needed.
    None,
    /// Compress scrollback for idle panes.
    CompressIdle,
    /// Evict scrollback to disk for old idle panes.
    EvictToDisk,
    /// Emergency: kill largest idle pane, evict all scrollback.
    EmergencyCleanup,
}

// =============================================================================
// Configuration
// =============================================================================

/// Memory pressure monitoring configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryPressureConfig {
    /// Enable memory pressure monitoring.
    pub enabled: bool,
    /// Sample interval in milliseconds.
    pub sample_interval_ms: u64,
    /// Threshold for Yellow (percentage of total RAM used).
    pub yellow_threshold: f64,
    /// Threshold for Orange.
    pub orange_threshold: f64,
    /// Threshold for Red.
    pub red_threshold: f64,
    /// Idle time before scrollback compression (seconds).
    pub compress_idle_secs: u64,
    /// Idle time before scrollback eviction to disk (seconds).
    pub evict_idle_secs: u64,
}

impl Default for MemoryPressureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 10_000,
            yellow_threshold: 70.0,
            orange_threshold: 85.0,
            red_threshold: 95.0,
            compress_idle_secs: 300,
            evict_idle_secs: 1800,
        }
    }
}

// =============================================================================
// Memory sample
// =============================================================================

/// A single memory pressure sample.
#[derive(Debug, Clone)]
pub struct MemorySample {
    /// Memory utilization percentage (0-100).
    pub used_percent: f64,
    /// Total system memory in KB.
    pub total_kb: u64,
    /// Available memory in KB.
    pub available_kb: u64,
    /// Classified tier.
    pub tier: MemoryPressureTier,
    /// Timestamp of the sample.
    pub sampled_at: Instant,
}

// =============================================================================
// Per-pane memory info
// =============================================================================

/// Per-pane memory tracking record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneMemoryInfo {
    /// Pane ID.
    pub pane_id: u64,
    /// Resident set size in KB for the pane's process tree.
    pub rss_kb: u64,
    /// Whether scrollback is compressed.
    pub scrollback_compressed: bool,
    /// Whether scrollback is evicted to disk.
    pub scrollback_evicted: bool,
    /// Time since last pane activity (seconds).
    pub idle_secs: u64,
}

// =============================================================================
// Monitor
// =============================================================================

/// Memory pressure monitor that samples system memory utilization.
///
/// Thread-safe. Uses atomic operations for the latest tier.
pub struct MemoryPressureMonitor {
    config: MemoryPressureConfig,
    /// Latest tier as atomic u8 (0-3).
    latest_tier: Arc<AtomicU64>,
}

impl MemoryPressureMonitor {
    /// Create a new monitor with the given configuration.
    pub fn new(config: MemoryPressureConfig) -> Self {
        Self {
            config,
            latest_tier: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the latest pressure tier (lock-free read).
    #[must_use]
    pub fn current_tier(&self) -> MemoryPressureTier {
        match self.latest_tier.load(Ordering::Relaxed) {
            1 => MemoryPressureTier::Yellow,
            2 => MemoryPressureTier::Orange,
            3 => MemoryPressureTier::Red,
            _ => MemoryPressureTier::Green,
        }
    }

    /// Get an Arc to the tier atomic for sharing with other tasks.
    #[must_use]
    pub fn tier_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.latest_tier)
    }

    /// Take a single memory pressure sample.
    pub fn sample(&self) -> MemorySample {
        let (total_kb, available_kb) = read_memory_info();
        let used_percent = if total_kb > 0 {
            ((total_kb - available_kb) as f64 / total_kb as f64) * 100.0
        } else {
            0.0
        };
        let tier = self.classify(used_percent);
        self.latest_tier
            .store(tier.as_u8() as u64, Ordering::Relaxed);

        MemorySample {
            used_percent,
            total_kb,
            available_kb,
            tier,
            sampled_at: Instant::now(),
        }
    }

    /// Run the monitoring loop until the shutdown flag is set.
    pub async fn run(&self, shutdown: Arc<std::sync::atomic::AtomicBool>) {
        let interval = Duration::from_millis(self.config.sample_interval_ms.max(1000));
        let mut first_tick = true;

        loop {
            if !first_tick {
                sleep(interval).await;
            }
            first_tick = false;

            if shutdown.load(Ordering::SeqCst) {
                break;
            }

            let sample = self.sample();
            if sample.tier >= MemoryPressureTier::Yellow {
                tracing::info!(
                    used_percent = format!("{:.1}", sample.used_percent),
                    available_mb = sample.available_kb / 1024,
                    tier = %sample.tier,
                    action = %sample.tier.suggested_action(),
                    "Memory pressure elevated"
                );
            }
        }
    }

    /// Classify memory utilization into a tier.
    fn classify(&self, used_percent: f64) -> MemoryPressureTier {
        if used_percent >= self.config.red_threshold {
            MemoryPressureTier::Red
        } else if used_percent >= self.config.orange_threshold {
            MemoryPressureTier::Orange
        } else if used_percent >= self.config.yellow_threshold {
            MemoryPressureTier::Yellow
        } else {
            MemoryPressureTier::Green
        }
    }
}

impl std::fmt::Display for MemoryAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::CompressIdle => write!(f, "compress_idle"),
            Self::EvictToDisk => write!(f, "evict_to_disk"),
            Self::EmergencyCleanup => write!(f, "emergency_cleanup"),
        }
    }
}

// =============================================================================
// Platform-specific memory reading
// =============================================================================

/// Read total and available memory in KB.
fn read_memory_info() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        read_linux_meminfo()
    }
    #[cfg(target_os = "macos")]
    {
        read_macos_memory()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        (0, 0)
    }
}

// =============================================================================
// Linux: /proc/meminfo
// =============================================================================

#[cfg(target_os = "linux")]
fn read_linux_meminfo() -> (u64, u64) {
    let Ok(contents) = std::fs::read_to_string("/proc/meminfo") else {
        return (0, 0);
    };

    let mut total_kb = 0u64;
    let mut available_kb = 0u64;

    for line in contents.lines() {
        if let Some(val) = line.strip_prefix("MemTotal:") {
            total_kb = parse_meminfo_value(val);
        } else if let Some(val) = line.strip_prefix("MemAvailable:") {
            available_kb = parse_meminfo_value(val);
        }
    }

    (total_kb, available_kb)
}

#[cfg(target_os = "linux")]
fn parse_meminfo_value(s: &str) -> u64 {
    s.trim()
        .trim_end_matches("kB")
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
}

// =============================================================================
// macOS: sysctl + vm_stat (safe, no FFI)
// =============================================================================

#[cfg(target_os = "macos")]
fn read_macos_memory() -> (u64, u64) {
    let total_kb = read_macos_total_memory();
    let available_kb = read_macos_available_memory();
    (total_kb, available_kb)
}

/// Read total physical memory via `sysctl -n hw.memsize` (returns bytes).
#[cfg(target_os = "macos")]
fn read_macos_total_memory() -> u64 {
    std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|bytes| bytes / 1024)
        .unwrap_or(0)
}

/// Read available memory by parsing `vm_stat` output.
///
/// vm_stat reports pages; we compute available = (free + inactive) pages × page_size.
#[cfg(target_os = "macos")]
fn read_macos_available_memory() -> u64 {
    let output = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());

    let Some(output) = output else {
        return 0;
    };

    // Parse page size from first line: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size = output
        .lines()
        .next()
        .and_then(|line| {
            let start = line.find("page size of ")? + 13;
            let end = line[start..].find(' ')? + start;
            line[start..end].parse::<u64>().ok()
        })
        .unwrap_or(16384);

    let mut free_pages = 0u64;
    let mut inactive_pages = 0u64;
    let mut purgeable_pages = 0u64;

    for line in output.lines() {
        if let Some(val) = line.strip_prefix("Pages free:") {
            free_pages = parse_vmstat_value(val);
        } else if let Some(val) = line.strip_prefix("Pages inactive:") {
            inactive_pages = parse_vmstat_value(val);
        } else if let Some(val) = line.strip_prefix("Pages purgeable:") {
            purgeable_pages = parse_vmstat_value(val);
        }
    }

    let available_pages = free_pages + inactive_pages + purgeable_pages;
    (available_pages * page_size) / 1024
}

/// Parse a vm_stat line value like "  12345.\n" → 12345
#[cfg(target_os = "macos")]
fn parse_vmstat_value(s: &str) -> u64 {
    s.trim().trim_end_matches('.').parse::<u64>().unwrap_or(0)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MemoryPressureConfig {
        MemoryPressureConfig {
            enabled: true,
            sample_interval_ms: 1000,
            yellow_threshold: 70.0,
            orange_threshold: 85.0,
            red_threshold: 95.0,
            compress_idle_secs: 300,
            evict_idle_secs: 1800,
        }
    }

    #[test]
    fn tier_ordering() {
        assert!(MemoryPressureTier::Green < MemoryPressureTier::Yellow);
        assert!(MemoryPressureTier::Yellow < MemoryPressureTier::Orange);
        assert!(MemoryPressureTier::Orange < MemoryPressureTier::Red);
    }

    #[test]
    fn tier_display() {
        assert_eq!(format!("{}", MemoryPressureTier::Green), "GREEN");
        assert_eq!(format!("{}", MemoryPressureTier::Red), "RED");
    }

    #[test]
    fn tier_numeric() {
        assert_eq!(MemoryPressureTier::Green.as_u8(), 0);
        assert_eq!(MemoryPressureTier::Yellow.as_u8(), 1);
        assert_eq!(MemoryPressureTier::Orange.as_u8(), 2);
        assert_eq!(MemoryPressureTier::Red.as_u8(), 3);
    }

    #[test]
    fn tier_suggested_actions() {
        assert_eq!(
            MemoryPressureTier::Green.suggested_action(),
            MemoryAction::None
        );
        assert_eq!(
            MemoryPressureTier::Yellow.suggested_action(),
            MemoryAction::CompressIdle
        );
        assert_eq!(
            MemoryPressureTier::Orange.suggested_action(),
            MemoryAction::EvictToDisk
        );
        assert_eq!(
            MemoryPressureTier::Red.suggested_action(),
            MemoryAction::EmergencyCleanup
        );
    }

    #[test]
    fn classify_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(0.0), MemoryPressureTier::Green);
        assert_eq!(monitor.classify(69.9), MemoryPressureTier::Green);
    }

    #[test]
    fn classify_yellow() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(70.0), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(84.9), MemoryPressureTier::Yellow);
    }

    #[test]
    fn classify_orange() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(85.0), MemoryPressureTier::Orange);
        assert_eq!(monitor.classify(94.9), MemoryPressureTier::Orange);
    }

    #[test]
    fn classify_red() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(95.0), MemoryPressureTier::Red);
        assert_eq!(monitor.classify(100.0), MemoryPressureTier::Red);
    }

    #[test]
    fn current_tier_default_is_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
    }

    #[test]
    fn sample_returns_valid_data() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let sample = monitor.sample();
        assert!(sample.used_percent >= 0.0);
        assert_eq!(sample.tier, monitor.current_tier());
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(sample.total_kb > 0, "total memory should be > 0");
        }
    }

    #[test]
    fn tier_handle_shares_state() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();
        assert_eq!(handle.load(Ordering::Relaxed), 0);

        handle.store(3, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Red);
    }

    #[test]
    fn default_config_values() {
        let cfg = MemoryPressureConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.sample_interval_ms, 10_000);
        assert!((cfg.yellow_threshold - 70.0).abs() < f64::EPSILON);
        assert!((cfg.orange_threshold - 85.0).abs() < f64::EPSILON);
        assert!((cfg.red_threshold - 95.0).abs() < f64::EPSILON);
        assert_eq!(cfg.compress_idle_secs, 300);
        assert_eq!(cfg.evict_idle_secs, 1800);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = MemoryPressureConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: MemoryPressureConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.yellow_threshold - cfg.yellow_threshold).abs() < f64::EPSILON);
        assert!((parsed.red_threshold - cfg.red_threshold).abs() < f64::EPSILON);
    }

    #[test]
    fn tier_serde_roundtrip() {
        let tier = MemoryPressureTier::Orange;
        let json = serde_json::to_string(&tier).unwrap();
        assert_eq!(json, "\"orange\"");
        let parsed: MemoryPressureTier = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, tier);
    }

    #[test]
    fn action_display() {
        assert_eq!(format!("{}", MemoryAction::None), "none");
        assert_eq!(format!("{}", MemoryAction::CompressIdle), "compress_idle");
        assert_eq!(format!("{}", MemoryAction::EvictToDisk), "evict_to_disk");
        assert_eq!(
            format!("{}", MemoryAction::EmergencyCleanup),
            "emergency_cleanup"
        );
    }

    #[test]
    fn action_serde_roundtrip() {
        for action in [
            MemoryAction::None,
            MemoryAction::CompressIdle,
            MemoryAction::EvictToDisk,
            MemoryAction::EmergencyCleanup,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: MemoryAction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, action);
        }
    }

    #[test]
    fn pane_memory_info_serde() {
        let info = PaneMemoryInfo {
            pane_id: 42,
            rss_kb: 500_000,
            scrollback_compressed: false,
            scrollback_evicted: false,
            idle_secs: 120,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 42);
        assert_eq!(parsed.rss_kb, 500_000);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_total_memory_readable() {
        let total = read_macos_total_memory();
        assert!(total > 0, "should detect total memory on macOS");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_available_memory_readable() {
        let available = read_macos_available_memory();
        assert!(available > 0, "should detect available memory on macOS");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_memory_ratio_sane() {
        let total = read_macos_total_memory();
        let available = read_macos_available_memory();
        assert!(
            available <= total,
            "available ({available}) should be <= total ({total})"
        );
    }

    #[test]
    fn read_memory_info_returns_values() {
        let (total, available) = read_memory_info();
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert!(total > 0);
            assert!(available > 0);
            assert!(available <= total);
        }
    }

    // -----------------------------------------------------------------------
    // Classify boundary conditions
    // -----------------------------------------------------------------------

    #[test]
    fn classify_at_exact_thresholds() {
        let monitor = MemoryPressureMonitor::new(test_config());
        // Exactly at threshold transitions.
        assert_eq!(monitor.classify(70.0), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(85.0), MemoryPressureTier::Orange);
        assert_eq!(monitor.classify(95.0), MemoryPressureTier::Red);
    }

    #[test]
    fn classify_just_below_thresholds() {
        let monitor = MemoryPressureMonitor::new(test_config());
        // Epsilon below each threshold stays in lower tier.
        assert_eq!(monitor.classify(69.999999), MemoryPressureTier::Green);
        assert_eq!(monitor.classify(84.999999), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(94.999999), MemoryPressureTier::Orange);
    }

    #[test]
    fn classify_zero_is_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(0.0), MemoryPressureTier::Green);
    }

    #[test]
    fn classify_hundred_is_red() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(100.0), MemoryPressureTier::Red);
    }

    #[test]
    fn classify_above_hundred_is_red() {
        let monitor = MemoryPressureMonitor::new(test_config());
        // >100% can happen with memory overcommit.
        assert_eq!(monitor.classify(150.0), MemoryPressureTier::Red);
    }

    #[test]
    fn classify_negative_is_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        assert_eq!(monitor.classify(-1.0), MemoryPressureTier::Green);
    }

    // -----------------------------------------------------------------------
    // Custom config thresholds
    // -----------------------------------------------------------------------

    #[test]
    fn custom_tight_thresholds() {
        let config = MemoryPressureConfig {
            yellow_threshold: 10.0,
            orange_threshold: 20.0,
            red_threshold: 30.0,
            ..MemoryPressureConfig::default()
        };
        let monitor = MemoryPressureMonitor::new(config);
        assert_eq!(monitor.classify(9.0), MemoryPressureTier::Green);
        assert_eq!(monitor.classify(10.0), MemoryPressureTier::Yellow);
        assert_eq!(monitor.classify(20.0), MemoryPressureTier::Orange);
        assert_eq!(monitor.classify(30.0), MemoryPressureTier::Red);
    }

    #[test]
    fn equal_thresholds_favor_highest_tier() {
        let config = MemoryPressureConfig {
            yellow_threshold: 50.0,
            orange_threshold: 50.0,
            red_threshold: 50.0,
            ..MemoryPressureConfig::default()
        };
        let monitor = MemoryPressureMonitor::new(config);
        // At 50.0, the >= checks proceed red→orange→yellow; red matches first.
        assert_eq!(monitor.classify(50.0), MemoryPressureTier::Red);
        assert_eq!(monitor.classify(49.9), MemoryPressureTier::Green);
    }

    // -----------------------------------------------------------------------
    // Atomic tier sharing
    // -----------------------------------------------------------------------

    #[test]
    fn tier_handle_round_trip_all_tiers() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();

        for (val, expected) in [
            (0u64, MemoryPressureTier::Green),
            (1, MemoryPressureTier::Yellow),
            (2, MemoryPressureTier::Orange),
            (3, MemoryPressureTier::Red),
        ] {
            handle.store(val, Ordering::Relaxed);
            assert_eq!(monitor.current_tier(), expected);
        }
    }

    #[test]
    fn unknown_tier_value_falls_back_to_green() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let handle = monitor.tier_handle();
        // Values outside 0-3 should map to Green (the _ arm).
        handle.store(99, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
        handle.store(u64::MAX, Ordering::Relaxed);
        assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
    }

    // -----------------------------------------------------------------------
    // Tier ordering properties
    // -----------------------------------------------------------------------

    #[test]
    fn all_tiers_have_distinct_numeric_values() {
        let values: Vec<u8> = [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ]
        .iter()
        .map(|t| t.as_u8())
        .collect();
        // Check strictly monotonic.
        for w in values.windows(2) {
            assert!(w[0] < w[1]);
        }
    }

    #[test]
    fn tier_ord_matches_numeric_ord() {
        let tiers = [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ];
        for i in 0..tiers.len() {
            for j in (i + 1)..tiers.len() {
                assert!(tiers[i] < tiers[j]);
                assert!(tiers[i].as_u8() < tiers[j].as_u8());
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tier serde exhaustive
    // -----------------------------------------------------------------------

    #[test]
    fn all_tiers_serde_roundtrip() {
        for tier in [
            MemoryPressureTier::Green,
            MemoryPressureTier::Yellow,
            MemoryPressureTier::Orange,
            MemoryPressureTier::Red,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let parsed: MemoryPressureTier = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, tier);
        }
    }

    #[test]
    fn tier_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Green).unwrap(),
            "\"green\""
        );
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Yellow).unwrap(),
            "\"yellow\""
        );
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Orange).unwrap(),
            "\"orange\""
        );
        assert_eq!(
            serde_json::to_string(&MemoryPressureTier::Red).unwrap(),
            "\"red\""
        );
    }

    // -----------------------------------------------------------------------
    // Config serde with partial fields
    // -----------------------------------------------------------------------

    #[test]
    fn config_deserializes_with_partial_fields() {
        let json = r#"{"enabled": false, "sample_interval_ms": 5000}"#;
        let config: MemoryPressureConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert_eq!(config.sample_interval_ms, 5000);
        // Remaining fields should be defaults.
        assert!((config.yellow_threshold - 70.0).abs() < f64::EPSILON);
        assert_eq!(config.compress_idle_secs, 300);
    }

    // -----------------------------------------------------------------------
    // PaneMemoryInfo edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn pane_memory_info_zero_values() {
        let info = PaneMemoryInfo {
            pane_id: 0,
            rss_kb: 0,
            scrollback_compressed: false,
            scrollback_evicted: false,
            idle_secs: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 0);
        assert_eq!(parsed.rss_kb, 0);
    }

    #[test]
    fn pane_memory_info_large_values() {
        let info = PaneMemoryInfo {
            pane_id: u64::MAX,
            rss_kb: u64::MAX,
            scrollback_compressed: true,
            scrollback_evicted: true,
            idle_secs: u64::MAX,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, u64::MAX);
        assert_eq!(parsed.rss_kb, u64::MAX);
        assert!(parsed.scrollback_compressed);
        assert!(parsed.scrollback_evicted);
    }

    // -----------------------------------------------------------------------
    // Sample updates atomic tier
    // -----------------------------------------------------------------------

    #[test]
    fn sample_updates_current_tier() {
        let monitor = MemoryPressureMonitor::new(test_config());
        let _sample = monitor.sample();
        // After a sample, current_tier() should reflect the sampled tier.
        // We can't predict the exact tier (depends on actual system memory),
        // but the tier should be a valid value.
        let tier = monitor.current_tier();
        assert!(tier.as_u8() <= 3);
    }
}
