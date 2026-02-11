//! Per-pane memory budget enforcement and OOM prevention.
//!
//! Provides per-pane memory budgets to prevent the mux server from being
//! OOM-killed when agents consume unbounded memory.
//!
//! - **Linux**: Uses cgroups v2 filesystem to create per-pane child cgroups
//!   under `/sys/fs/cgroup/frankenterm/`, setting `memory.max` (hard limit)
//!   and `memory.high` (soft throttle). Reads `memory.current` for RSS.
//! - **macOS**: Advisory budget tracking — no cgroups equivalent exists.
//!   Tracks per-pane budgets in-process and compares against process RSS
//!   via `ps -o rss=`. Memory pressure detection via `vm_stat`.
//! - **Other**: Graceful no-op fallback.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

// =============================================================================
// Budget enforcement level
// =============================================================================

/// How a pane is responding to its memory budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetLevel {
    /// Usage well within budget.
    Normal,
    /// Usage approaching budget — soft throttle active (memory.high).
    Throttled,
    /// Usage at or above budget — hard limit hit (memory.max).
    OverBudget,
}

impl std::fmt::Display for BudgetLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(f, "NORMAL"),
            Self::Throttled => write!(f, "THROTTLED"),
            Self::OverBudget => write!(f, "OVER_BUDGET"),
        }
    }
}

impl BudgetLevel {
    /// Numeric value for metrics (0-2).
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Throttled => 1,
            Self::OverBudget => 2,
        }
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Per-pane memory budget configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryBudgetConfig {
    /// Enable per-pane memory budget enforcement.
    pub enabled: bool,
    /// Default per-pane memory budget in bytes (default: 1 GiB).
    pub default_budget_bytes: u64,
    /// Soft limit ratio — fraction of budget that triggers throttling.
    /// memory.high = budget * high_ratio (default: 0.8 = 80%).
    pub high_ratio: f64,
    /// Monitoring sample interval in milliseconds.
    pub sample_interval_ms: u64,
    /// Base path for the FrankenTerm cgroup hierarchy (Linux only).
    pub cgroup_base_path: String,
    /// Whether to use cgroups v2 on Linux (set false to use advisory mode).
    pub use_cgroups: bool,
    /// OOM score adjustment for the mux server process (Linux only).
    /// Lower values make the kernel less likely to OOM-kill us.
    /// Range: -1000 to 1000. Default: -500 (protect mux server).
    pub oom_score_adj: i32,
}

impl Default for MemoryBudgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_budget_bytes: 1024 * 1024 * 1024, // 1 GiB
            high_ratio: 0.8,
            sample_interval_ms: 5000,
            cgroup_base_path: "/sys/fs/cgroup/frankenterm".to_string(),
            use_cgroups: true,
            oom_score_adj: -500,
        }
    }
}

// =============================================================================
// Per-pane budget state
// =============================================================================

/// Tracked state for a single pane's memory budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBudget {
    /// Pane ID.
    pub pane_id: u64,
    /// Budget cap in bytes.
    pub budget_bytes: u64,
    /// Soft throttle threshold in bytes (budget * high_ratio).
    pub high_bytes: u64,
    /// Current memory usage in bytes (last sampled).
    pub current_bytes: u64,
    /// Current budget enforcement level.
    pub level: BudgetLevel,
    /// Whether a cgroup was successfully created for this pane (Linux only).
    pub cgroup_active: bool,
    /// Process ID associated with this pane (for RSS lookup on macOS).
    pub pid: Option<u32>,
}

impl PaneBudget {
    /// Create a new budget entry for a pane.
    fn new(pane_id: u64, budget_bytes: u64, high_ratio: f64) -> Self {
        let high_bytes = (budget_bytes as f64 * high_ratio) as u64;
        Self {
            pane_id,
            budget_bytes,
            high_bytes,
            current_bytes: 0,
            level: BudgetLevel::Normal,
            cgroup_active: false,
            pid: None,
        }
    }

    /// Update the budget level based on current usage.
    fn update_level(&mut self) {
        self.level = if self.current_bytes >= self.budget_bytes {
            BudgetLevel::OverBudget
        } else if self.current_bytes >= self.high_bytes {
            BudgetLevel::Throttled
        } else {
            BudgetLevel::Normal
        };
    }

    /// Usage as a fraction of budget (0.0-1.0+).
    #[must_use]
    pub fn usage_ratio(&self) -> f64 {
        if self.budget_bytes == 0 {
            return 0.0;
        }
        self.current_bytes as f64 / self.budget_bytes as f64
    }
}

// =============================================================================
// Budget summary
// =============================================================================

/// Aggregate summary of all pane memory budgets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetSummary {
    /// Number of tracked panes.
    pub pane_count: usize,
    /// Total budget allocated across all panes.
    pub total_budget_bytes: u64,
    /// Total current usage across all panes.
    pub total_current_bytes: u64,
    /// Number of panes at each budget level.
    pub normal_count: usize,
    pub throttled_count: usize,
    pub over_budget_count: usize,
    /// Pane with highest usage ratio (None if no panes).
    pub worst_pane_id: Option<u64>,
    /// Highest usage ratio across all panes.
    pub worst_usage_ratio: f64,
}

// =============================================================================
// Manager
// =============================================================================

/// Memory budget manager that tracks per-pane budgets.
///
/// Thread-safe. Internal state protected by a Mutex; the worst budget level
/// is also available as a lock-free atomic read.
pub struct MemoryBudgetManager {
    config: MemoryBudgetConfig,
    /// Per-pane budget tracking.
    panes: Mutex<HashMap<u64, PaneBudget>>,
    /// Worst (highest) budget level across all panes, as atomic u8.
    worst_level: Arc<AtomicU64>,
}

impl MemoryBudgetManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: MemoryBudgetConfig) -> Self {
        Self {
            config,
            panes: Mutex::new(HashMap::new()),
            worst_level: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Get the worst budget level across all panes (lock-free).
    #[must_use]
    pub fn worst_level(&self) -> BudgetLevel {
        match self.worst_level.load(Ordering::Relaxed) {
            1 => BudgetLevel::Throttled,
            2 => BudgetLevel::OverBudget,
            _ => BudgetLevel::Normal,
        }
    }

    /// Get an Arc to the worst-level atomic for sharing with other tasks.
    #[must_use]
    pub fn level_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.worst_level)
    }

    /// Register a new pane with the default budget.
    ///
    /// On Linux with cgroups enabled, creates the cgroup hierarchy.
    /// On macOS, tracks the budget in-process only.
    pub fn register_pane(&self, pane_id: u64, pid: Option<u32>) -> PaneBudget {
        self.register_pane_with_budget(pane_id, pid, self.config.default_budget_bytes)
    }

    /// Register a new pane with a custom budget.
    pub fn register_pane_with_budget(
        &self,
        pane_id: u64,
        pid: Option<u32>,
        budget_bytes: u64,
    ) -> PaneBudget {
        let mut budget = PaneBudget::new(pane_id, budget_bytes, self.config.high_ratio);
        budget.pid = pid;

        // Attempt cgroup creation on Linux
        #[cfg(target_os = "linux")]
        if self.config.use_cgroups {
            budget.cgroup_active = create_pane_cgroup(
                &self.config.cgroup_base_path,
                pane_id,
                budget_bytes,
                budget.high_bytes,
            );
        }

        let result = budget.clone();
        let mut panes = self.panes.lock().unwrap_or_else(|e| e.into_inner());
        panes.insert(pane_id, budget);
        result
    }

    /// Unregister a pane and clean up its cgroup (if applicable).
    pub fn unregister_pane(&self, pane_id: u64) -> Option<PaneBudget> {
        let mut panes = self.panes.lock().unwrap_or_else(|e| e.into_inner());
        let removed = panes.remove(&pane_id);

        #[cfg(target_os = "linux")]
        if let Some(ref budget) = removed {
            if budget.cgroup_active {
                destroy_pane_cgroup(&self.config.cgroup_base_path, pane_id);
            }
        }

        self.update_worst_level_from(&panes);
        removed
    }

    /// Sample current memory usage for all tracked panes and update levels.
    pub fn sample_all(&self) -> BudgetSummary {
        let mut panes = self.panes.lock().unwrap_or_else(|e| e.into_inner());

        for budget in panes.values_mut() {
            budget.current_bytes = read_pane_memory(
                &self.config,
                budget.pane_id,
                budget.pid,
                budget.cgroup_active,
            );
            budget.update_level();
        }

        let summary = compute_summary(&panes);
        self.update_worst_level_from(&panes);
        summary
    }

    /// Get the current budget state for a specific pane.
    #[must_use]
    pub fn get_pane_budget(&self, pane_id: u64) -> Option<PaneBudget> {
        let panes = self.panes.lock().unwrap_or_else(|e| e.into_inner());
        panes.get(&pane_id).cloned()
    }

    /// Get a snapshot of all pane budgets.
    #[must_use]
    pub fn all_pane_budgets(&self) -> Vec<PaneBudget> {
        let panes = self.panes.lock().unwrap_or_else(|e| e.into_inner());
        panes.values().cloned().collect()
    }

    /// Get configuration reference.
    #[must_use]
    pub fn config(&self) -> &MemoryBudgetConfig {
        &self.config
    }

    /// Update the worst level atomic from the current pane map.
    fn update_worst_level_from(&self, panes: &HashMap<u64, PaneBudget>) {
        let worst = panes
            .values()
            .map(|b| b.level)
            .max()
            .unwrap_or(BudgetLevel::Normal);
        self.worst_level
            .store(worst.as_u8() as u64, Ordering::Relaxed);
    }

    /// Run the monitoring loop until the shutdown flag is set.
    pub async fn run(&self, shutdown: Arc<std::sync::atomic::AtomicBool>) {
        let interval = std::time::Duration::from_millis(self.config.sample_interval_ms.max(1000));
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;
            if shutdown.load(Ordering::SeqCst) {
                break;
            }

            let summary = self.sample_all();
            if summary.throttled_count > 0 || summary.over_budget_count > 0 {
                tracing::warn!(
                    throttled = summary.throttled_count,
                    over_budget = summary.over_budget_count,
                    worst_pane = ?summary.worst_pane_id,
                    worst_ratio = format!("{:.2}", summary.worst_usage_ratio),
                    "Memory budget pressure detected"
                );
            }
        }
    }

    /// Protect the mux server process from OOM kill (Linux only).
    ///
    /// Writes to `/proc/self/oom_score_adj` to lower our OOM priority.
    /// Returns true if successful.
    #[must_use]
    pub fn protect_mux_server(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            protect_oom_score(self.config.oom_score_adj)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

// =============================================================================
// Summary computation
// =============================================================================

fn compute_summary(panes: &HashMap<u64, PaneBudget>) -> BudgetSummary {
    let mut total_budget = 0u64;
    let mut total_current = 0u64;
    let mut normal = 0usize;
    let mut throttled = 0usize;
    let mut over_budget = 0usize;
    let mut worst_id = None;
    let mut worst_ratio = 0.0f64;

    for budget in panes.values() {
        total_budget = total_budget.saturating_add(budget.budget_bytes);
        total_current = total_current.saturating_add(budget.current_bytes);

        match budget.level {
            BudgetLevel::Normal => normal += 1,
            BudgetLevel::Throttled => throttled += 1,
            BudgetLevel::OverBudget => over_budget += 1,
        }

        let ratio = budget.usage_ratio();
        if ratio > worst_ratio {
            worst_ratio = ratio;
            worst_id = Some(budget.pane_id);
        }
    }

    BudgetSummary {
        pane_count: panes.len(),
        total_budget_bytes: total_budget,
        total_current_bytes: total_current,
        normal_count: normal,
        throttled_count: throttled,
        over_budget_count: over_budget,
        worst_pane_id: worst_id,
        worst_usage_ratio: worst_ratio,
    }
}

// =============================================================================
// Platform: read per-pane memory usage
// =============================================================================

/// Read current memory usage for a pane.
fn read_pane_memory(
    config: &MemoryBudgetConfig,
    pane_id: u64,
    pid: Option<u32>,
    cgroup_active: bool,
) -> u64 {
    #[cfg(target_os = "linux")]
    if cgroup_active {
        if let Some(bytes) = read_cgroup_memory_current(&config.cgroup_base_path, pane_id) {
            return bytes;
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(p) = pid {
        return read_pid_rss(p);
    }

    let _ = (config, pane_id, pid, cgroup_active);
    0
}

/// Read RSS for a given PID via `ps -o rss= -p <pid>` (returns bytes).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_pid_rss(pid: u32) -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

// =============================================================================
// Linux: cgroups v2 filesystem operations
// =============================================================================

#[cfg(target_os = "linux")]
fn create_pane_cgroup(base: &str, pane_id: u64, max_bytes: u64, high_bytes: u64) -> bool {
    let cgroup_path = format!("{base}/pane-{pane_id}");

    if std::fs::create_dir_all(&cgroup_path).is_err() {
        tracing::warn!(pane_id, path = %cgroup_path, "Failed to create pane cgroup directory");
        return false;
    }

    let max_path = format!("{cgroup_path}/memory.max");
    if std::fs::write(&max_path, max_bytes.to_string()).is_err() {
        tracing::warn!(pane_id, path = %max_path, "Failed to write memory.max");
        return false;
    }

    let high_path = format!("{cgroup_path}/memory.high");
    if std::fs::write(&high_path, high_bytes.to_string()).is_err() {
        tracing::warn!(pane_id, path = %high_path, "Failed to write memory.high");
        return false;
    }

    tracing::info!(
        pane_id,
        max_mb = max_bytes / (1024 * 1024),
        high_mb = high_bytes / (1024 * 1024),
        "Created pane cgroup with memory limits"
    );
    true
}

#[cfg(target_os = "linux")]
fn destroy_pane_cgroup(base: &str, pane_id: u64) {
    let cgroup_path = format!("{base}/pane-{pane_id}");
    if let Err(e) = std::fs::remove_dir_all(&cgroup_path) {
        tracing::debug!(pane_id, error = %e, "Failed to remove pane cgroup (may already be gone)");
    }
}

#[cfg(target_os = "linux")]
fn read_cgroup_memory_current(base: &str, pane_id: u64) -> Option<u64> {
    let path = format!("{base}/pane-{pane_id}/memory.current");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
}

#[cfg(target_os = "linux")]
fn protect_oom_score(adj: i32) -> bool {
    let clamped = adj.clamp(-1000, 1000);
    std::fs::write("/proc/self/oom_score_adj", clamped.to_string()).is_ok()
}

// =============================================================================
// macOS: advisory memory helpers
// =============================================================================

/// Check macOS system memory pressure level.
#[cfg(target_os = "macos")]
pub fn macos_system_memory() -> (u64, u64) {
    let total = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);

    let available = macos_available_bytes();
    (total, available)
}

#[cfg(target_os = "macos")]
fn macos_available_bytes() -> u64 {
    let output = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok());

    let Some(output) = output else {
        return 0;
    };

    let page_size = output
        .lines()
        .next()
        .and_then(|line| {
            let start = line.find("page size of ")? + 13;
            let end = line[start..].find(' ')? + start;
            line[start..end].parse::<u64>().ok()
        })
        .unwrap_or(16384);

    let mut free = 0u64;
    let mut inactive = 0u64;
    let mut purgeable = 0u64;

    for line in output.lines() {
        if let Some(val) = line.strip_prefix("Pages free:") {
            free = parse_vmstat_pages(val);
        } else if let Some(val) = line.strip_prefix("Pages inactive:") {
            inactive = parse_vmstat_pages(val);
        } else if let Some(val) = line.strip_prefix("Pages purgeable:") {
            purgeable = parse_vmstat_pages(val);
        }
    }

    (free + inactive + purgeable) * page_size
}

#[cfg(target_os = "macos")]
fn parse_vmstat_pages(s: &str) -> u64 {
    s.trim().trim_end_matches('.').parse::<u64>().unwrap_or(0)
}

// =============================================================================
// Cgroups v2 availability check
// =============================================================================

/// Check whether cgroups v2 is available on the current system.
#[cfg(target_os = "linux")]
pub fn cgroups_v2_available() -> bool {
    std::fs::read_to_string("/sys/fs/cgroup/cgroup.controllers").is_ok()
}

#[cfg(not(target_os = "linux"))]
pub fn cgroups_v2_available() -> bool {
    false
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> MemoryBudgetConfig {
        MemoryBudgetConfig {
            enabled: true,
            default_budget_bytes: 512 * 1024 * 1024, // 512 MiB
            high_ratio: 0.8,
            sample_interval_ms: 1000,
            cgroup_base_path: "/tmp/frankenterm-test-cgroup".to_string(),
            use_cgroups: false,
            oom_score_adj: -500,
        }
    }

    // ---- BudgetLevel ----

    #[test]
    fn budget_level_ordering() {
        assert!(BudgetLevel::Normal < BudgetLevel::Throttled);
        assert!(BudgetLevel::Throttled < BudgetLevel::OverBudget);
    }

    #[test]
    fn budget_level_display() {
        assert_eq!(format!("{}", BudgetLevel::Normal), "NORMAL");
        assert_eq!(format!("{}", BudgetLevel::Throttled), "THROTTLED");
        assert_eq!(format!("{}", BudgetLevel::OverBudget), "OVER_BUDGET");
    }

    #[test]
    fn budget_level_numeric() {
        assert_eq!(BudgetLevel::Normal.as_u8(), 0);
        assert_eq!(BudgetLevel::Throttled.as_u8(), 1);
        assert_eq!(BudgetLevel::OverBudget.as_u8(), 2);
    }

    #[test]
    fn budget_level_serde_roundtrip() {
        for level in [
            BudgetLevel::Normal,
            BudgetLevel::Throttled,
            BudgetLevel::OverBudget,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let parsed: BudgetLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, level);
        }
    }

    // ---- MemoryBudgetConfig ----

    #[test]
    fn default_config_values() {
        let cfg = MemoryBudgetConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.default_budget_bytes, 1024 * 1024 * 1024);
        assert!((cfg.high_ratio - 0.8).abs() < f64::EPSILON);
        assert_eq!(cfg.sample_interval_ms, 5000);
        assert!(cfg.use_cgroups);
        assert_eq!(cfg.oom_score_adj, -500);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = MemoryBudgetConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: MemoryBudgetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.default_budget_bytes, cfg.default_budget_bytes);
        assert!((parsed.high_ratio - cfg.high_ratio).abs() < f64::EPSILON);
        assert_eq!(parsed.use_cgroups, cfg.use_cgroups);
    }

    // ---- PaneBudget ----

    #[test]
    fn pane_budget_new_calculates_high() {
        let budget = PaneBudget::new(1, 1000, 0.8);
        assert_eq!(budget.pane_id, 1);
        assert_eq!(budget.budget_bytes, 1000);
        assert_eq!(budget.high_bytes, 800);
        assert_eq!(budget.current_bytes, 0);
        assert_eq!(budget.level, BudgetLevel::Normal);
        assert!(!budget.cgroup_active);
        assert!(budget.pid.is_none());
    }

    #[test]
    fn pane_budget_update_level_normal() {
        let mut b = PaneBudget::new(1, 1000, 0.8);
        b.current_bytes = 500;
        b.update_level();
        assert_eq!(b.level, BudgetLevel::Normal);
    }

    #[test]
    fn pane_budget_update_level_throttled() {
        let mut b = PaneBudget::new(1, 1000, 0.8);
        b.current_bytes = 800;
        b.update_level();
        assert_eq!(b.level, BudgetLevel::Throttled);
    }

    #[test]
    fn pane_budget_update_level_over_budget() {
        let mut b = PaneBudget::new(1, 1000, 0.8);
        b.current_bytes = 1000;
        b.update_level();
        assert_eq!(b.level, BudgetLevel::OverBudget);
    }

    #[test]
    fn pane_budget_update_level_over_budget_exceeds() {
        let mut b = PaneBudget::new(1, 1000, 0.8);
        b.current_bytes = 1500;
        b.update_level();
        assert_eq!(b.level, BudgetLevel::OverBudget);
    }

    #[test]
    fn pane_budget_usage_ratio() {
        let mut b = PaneBudget::new(1, 1000, 0.8);
        b.current_bytes = 250;
        assert!((b.usage_ratio() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn pane_budget_usage_ratio_zero_budget() {
        let b = PaneBudget::new(1, 0, 0.8);
        assert!((b.usage_ratio() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn pane_budget_serde_roundtrip() {
        let mut b = PaneBudget::new(42, 1_000_000, 0.8);
        b.current_bytes = 500_000;
        b.pid = Some(12345);
        b.update_level();

        let json = serde_json::to_string(&b).unwrap();
        let parsed: PaneBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 42);
        assert_eq!(parsed.budget_bytes, 1_000_000);
        assert_eq!(parsed.current_bytes, 500_000);
        assert_eq!(parsed.pid, Some(12345));
    }

    // ---- MemoryBudgetManager ----

    #[test]
    fn manager_register_and_get_pane() {
        let mgr = MemoryBudgetManager::new(test_config());
        let budget = mgr.register_pane(1, Some(999));
        assert_eq!(budget.pane_id, 1);
        assert_eq!(budget.budget_bytes, 512 * 1024 * 1024);
        assert_eq!(budget.pid, Some(999));

        let fetched = mgr.get_pane_budget(1).unwrap();
        assert_eq!(fetched.pane_id, 1);
    }

    #[test]
    fn manager_register_with_custom_budget() {
        let mgr = MemoryBudgetManager::new(test_config());
        let budget = mgr.register_pane_with_budget(2, None, 256 * 1024 * 1024);
        assert_eq!(budget.budget_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn manager_unregister_pane() {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(1, None);
        assert!(mgr.get_pane_budget(1).is_some());

        let removed = mgr.unregister_pane(1);
        assert!(removed.is_some());
        assert!(mgr.get_pane_budget(1).is_none());
    }

    #[test]
    fn manager_unregister_missing_pane() {
        let mgr = MemoryBudgetManager::new(test_config());
        assert!(mgr.unregister_pane(999).is_none());
    }

    #[test]
    fn manager_worst_level_default() {
        let mgr = MemoryBudgetManager::new(test_config());
        assert_eq!(mgr.worst_level(), BudgetLevel::Normal);
    }

    #[test]
    fn manager_level_handle_shares_state() {
        let mgr = MemoryBudgetManager::new(test_config());
        let handle = mgr.level_handle();
        assert_eq!(handle.load(Ordering::Relaxed), 0);

        handle.store(2, Ordering::Relaxed);
        assert_eq!(mgr.worst_level(), BudgetLevel::OverBudget);
    }

    #[test]
    fn manager_all_pane_budgets() {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(1, None);
        mgr.register_pane(2, None);
        mgr.register_pane(3, None);

        let all = mgr.all_pane_budgets();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn manager_sample_all_empty() {
        let mgr = MemoryBudgetManager::new(test_config());
        let summary = mgr.sample_all();
        assert_eq!(summary.pane_count, 0);
        assert_eq!(summary.total_budget_bytes, 0);
        assert_eq!(summary.total_current_bytes, 0);
        assert_eq!(summary.normal_count, 0);
        assert!(summary.worst_pane_id.is_none());
    }

    #[test]
    fn manager_sample_all_with_panes() {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(1, None);
        mgr.register_pane(2, None);

        let summary = mgr.sample_all();
        assert_eq!(summary.pane_count, 2);
        assert_eq!(summary.total_budget_bytes, 2 * 512 * 1024 * 1024);
        assert_eq!(summary.normal_count, 2);
        assert_eq!(summary.throttled_count, 0);
        assert_eq!(summary.over_budget_count, 0);
    }

    #[test]
    fn manager_config_access() {
        let mgr = MemoryBudgetManager::new(test_config());
        assert_eq!(mgr.config().default_budget_bytes, 512 * 1024 * 1024);
    }

    // ---- Summary computation ----

    #[test]
    fn compute_summary_mixed_levels() {
        let mut panes = HashMap::new();

        let mut b1 = PaneBudget::new(1, 1000, 0.8);
        b1.current_bytes = 100;
        b1.update_level();
        panes.insert(1, b1);

        let mut b2 = PaneBudget::new(2, 1000, 0.8);
        b2.current_bytes = 900;
        b2.update_level();
        panes.insert(2, b2);

        let mut b3 = PaneBudget::new(3, 1000, 0.8);
        b3.current_bytes = 1100;
        b3.update_level();
        panes.insert(3, b3);

        let summary = compute_summary(&panes);
        assert_eq!(summary.pane_count, 3);
        assert_eq!(summary.total_budget_bytes, 3000);
        assert_eq!(summary.total_current_bytes, 2100);
        assert_eq!(summary.normal_count, 1);
        assert_eq!(summary.throttled_count, 1);
        assert_eq!(summary.over_budget_count, 1);
        assert_eq!(summary.worst_pane_id, Some(3));
        assert!((summary.worst_usage_ratio - 1.1).abs() < 0.01);
    }

    #[test]
    fn compute_summary_empty() {
        let panes = HashMap::new();
        let summary = compute_summary(&panes);
        assert_eq!(summary.pane_count, 0);
        assert!(summary.worst_pane_id.is_none());
        assert!((summary.worst_usage_ratio - 0.0).abs() < f64::EPSILON);
    }

    // ---- BudgetSummary serde ----

    #[test]
    fn budget_summary_serde_roundtrip() {
        let summary = BudgetSummary {
            pane_count: 3,
            total_budget_bytes: 3_000_000_000,
            total_current_bytes: 1_500_000_000,
            normal_count: 2,
            throttled_count: 1,
            over_budget_count: 0,
            worst_pane_id: Some(42),
            worst_usage_ratio: 0.85,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: BudgetSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_count, 3);
        assert_eq!(parsed.worst_pane_id, Some(42));
    }

    // ---- cgroups_v2_available ----

    #[test]
    fn cgroups_v2_available_returns_bool() {
        let _available = cgroups_v2_available();
    }

    // ---- Platform-specific tests ----

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_pid_rss_self() {
        let pid = std::process::id();
        let rss = read_pid_rss(pid);
        assert!(rss > 0, "should detect RSS for our own process");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn read_pid_rss_nonexistent() {
        let rss = read_pid_rss(999_999_999);
        assert_eq!(rss, 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_system_memory_readable() {
        let (total, available) = macos_system_memory();
        assert!(total > 0, "should detect total memory on macOS");
        assert!(available > 0, "should detect available memory on macOS");
        assert!(available <= total, "available should be <= total");
    }

    // ---- Integration-style tests ----

    #[test]
    fn manager_lifecycle_register_sample_unregister() {
        let mgr = MemoryBudgetManager::new(test_config());

        mgr.register_pane(1, None);
        mgr.register_pane(2, None);

        let summary = mgr.sample_all();
        assert_eq!(summary.pane_count, 2);

        mgr.unregister_pane(1);
        let summary = mgr.sample_all();
        assert_eq!(summary.pane_count, 1);

        mgr.unregister_pane(2);
        let summary = mgr.sample_all();
        assert_eq!(summary.pane_count, 0);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn manager_with_real_pid() {
        let mgr = MemoryBudgetManager::new(test_config());
        let pid = std::process::id();
        mgr.register_pane(1, Some(pid));

        let summary = mgr.sample_all();
        assert_eq!(summary.pane_count, 1);
        assert!(
            summary.total_current_bytes > 0,
            "should read RSS for real process"
        );
    }

    #[test]
    fn manager_protect_mux_server() {
        let mgr = MemoryBudgetManager::new(test_config());
        let _result = mgr.protect_mux_server();
    }

    #[test]
    fn worst_level_updates_after_manual_level_change() {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(1, None);

        {
            let mut panes = mgr.panes.lock().unwrap();
            if let Some(b) = panes.get_mut(&1) {
                b.current_bytes = b.high_bytes + 1;
                b.update_level();
            }
            mgr.update_worst_level_from(&panes);
        }

        assert_eq!(mgr.worst_level(), BudgetLevel::Throttled);
    }

    // ---- Linux cgroup filesystem tests (with temp dir) ----

    #[cfg(target_os = "linux")]
    mod linux_cgroup_tests {
        use super::*;

        #[test]
        fn create_and_read_cgroup_tempdir() {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().to_str().unwrap().to_string();

            let ok = create_pane_cgroup(&base, 42, 1_000_000, 800_000);
            assert!(ok, "should create cgroup in temp dir");

            let max_content =
                std::fs::read_to_string(format!("{base}/pane-42/memory.max")).unwrap();
            assert_eq!(max_content, "1000000");

            let high_content =
                std::fs::read_to_string(format!("{base}/pane-42/memory.high")).unwrap();
            assert_eq!(high_content, "800000");

            std::fs::write(format!("{base}/pane-42/memory.current"), "500000").unwrap();
            let current = read_cgroup_memory_current(&base, 42);
            assert_eq!(current, Some(500_000));

            destroy_pane_cgroup(&base, 42);
            assert!(!std::path::Path::new(&format!("{base}/pane-42")).exists());
        }

        #[test]
        fn read_nonexistent_cgroup() {
            let current = read_cgroup_memory_current("/nonexistent/path", 999);
            assert!(current.is_none());
        }

        #[test]
        fn manager_with_cgroups_tempdir() {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().to_str().unwrap().to_string();

            let config = MemoryBudgetConfig {
                enabled: true,
                default_budget_bytes: 1_000_000,
                high_ratio: 0.8,
                sample_interval_ms: 1000,
                cgroup_base_path: base.clone(),
                use_cgroups: true,
                oom_score_adj: 0,
            };

            let mgr = MemoryBudgetManager::new(config);
            let budget = mgr.register_pane(1, None);
            assert!(budget.cgroup_active);

            std::fs::write(format!("{base}/pane-1/memory.current"), "900000").unwrap();

            let summary = mgr.sample_all();
            assert_eq!(summary.pane_count, 1);
            assert!(summary.total_current_bytes > 0);
            assert_eq!(summary.throttled_count, 1);

            mgr.unregister_pane(1);
            assert!(!std::path::Path::new(&format!("{base}/pane-1")).exists());
        }
    }
}
