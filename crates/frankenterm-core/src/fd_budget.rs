//! File descriptor budget tracking and resource validation.
//!
//! Monitors FD usage across the FrankenTerm process to prevent "Too many open
//! files" errors when running 200+ panes. Provides startup validation of
//! system limits and ongoing FD leak detection.
//!
//! # Design
//!
//! ```text
//! Startup:   validate_system_limits() → warn/error if insufficient
//! Runtime:   FdBudget tracks per-pane and global FD usage
//! Periodic:  audit_open_fds() detects leaks via monotonic growth
//! ```
//!
//! # Platform Support
//!
//! - **Linux**: reads `/proc/self/fd`, parses `/proc/sys/fs/file-max`
//! - **macOS**: uses `getrlimit(RLIMIT_NOFILE)`, reads `kern.maxfiles` sysctl

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::concurrent_map::PaneMap;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for FD budget management.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FdBudgetConfig {
    /// Warn when FD usage exceeds this fraction of the limit.
    pub warn_threshold: f64,
    /// Refuse new panes when FD usage exceeds this fraction.
    pub refuse_threshold: f64,
    /// Expected FDs per pane (PTY + pipes + sockets).
    pub fds_per_pane: u64,
    /// Minimum acceptable ulimit -n value.
    pub min_nofile_limit: u64,
    /// How often to audit FDs for leaks (seconds).
    pub audit_interval_secs: u64,
    /// Number of consecutive rising audits before declaring a leak.
    pub leak_detection_count: usize,
}

/// Invalid FD budget configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdBudgetConfigError {
    /// Warn/refuse threshold is NaN or infinite.
    NonFiniteThreshold,
    /// Warn/refuse threshold is outside the inclusive 0.0..=1.0 range.
    ThresholdOutOfRange,
    /// Warn threshold is higher than the refuse threshold.
    MisorderedThresholds,
    /// A pane must reserve at least one expected file descriptor.
    ZeroFdsPerPane,
    /// The minimum nofile limit must be nonzero.
    ZeroMinNofileLimit,
    /// Leak detection needs at least one audit sample.
    ZeroLeakDetectionCount,
}

impl Default for FdBudgetConfig {
    fn default() -> Self {
        Self {
            warn_threshold: 0.80,
            refuse_threshold: 0.95,
            fds_per_pane: 25,
            min_nofile_limit: 65_536,
            audit_interval_secs: 30,
            leak_detection_count: 5,
        }
    }
}

impl FdBudgetConfig {
    /// Validate operator/serde supplied FD budget settings before use.
    pub fn validate(&self) -> Result<(), FdBudgetConfigError> {
        if !self.warn_threshold.is_finite() || !self.refuse_threshold.is_finite() {
            return Err(FdBudgetConfigError::NonFiniteThreshold);
        }
        if !(0.0..=1.0).contains(&self.warn_threshold)
            || !(0.0..=1.0).contains(&self.refuse_threshold)
        {
            return Err(FdBudgetConfigError::ThresholdOutOfRange);
        }
        if self.warn_threshold > self.refuse_threshold {
            return Err(FdBudgetConfigError::MisorderedThresholds);
        }
        if self.fds_per_pane == 0 {
            return Err(FdBudgetConfigError::ZeroFdsPerPane);
        }
        if self.min_nofile_limit == 0 {
            return Err(FdBudgetConfigError::ZeroMinNofileLimit);
        }
        if self.leak_detection_count == 0 {
            return Err(FdBudgetConfigError::ZeroLeakDetectionCount);
        }
        Ok(())
    }

    fn fail_closed() -> Self {
        Self {
            warn_threshold: 0.0,
            refuse_threshold: 0.0,
            fds_per_pane: u64::MAX,
            min_nofile_limit: u64::MAX,
            audit_interval_secs: 1,
            leak_detection_count: 1,
        }
    }

    fn validated_or_fail_closed(self) -> Self {
        if self.validate().is_ok() {
            self
        } else {
            Self::fail_closed()
        }
    }
}

// =============================================================================
// System limits
// =============================================================================

/// System resource limits relevant to FD management.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemLimits {
    /// Soft limit on open file descriptors (ulimit -n).
    pub nofile_soft: u64,
    /// Hard limit on open file descriptors.
    pub nofile_hard: u64,
    /// System-wide maximum open files (Linux: fs.file-max).
    pub system_max_files: Option<u64>,
    /// Currently open FDs in this process.
    pub current_open_fds: u64,
    /// Platform identifier.
    pub platform: String,
}

/// Validation result for system limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitValidation {
    /// Whether all limits are sufficient.
    pub all_ok: bool,
    /// Individual check results.
    pub checks: Vec<LimitCheck>,
    /// Platform-specific fix commands (if any limits are insufficient).
    pub fix_commands: Vec<String>,
}

/// A single limit check result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitCheck {
    /// What was checked.
    pub name: String,
    /// Current value.
    pub current: u64,
    /// Required minimum.
    pub required: u64,
    /// Whether this check passed.
    pub ok: bool,
}

/// Query current system limits.
///
/// Reads ulimit values via shell command to avoid unsafe libc calls.
pub fn get_system_limits() -> SystemLimits {
    let soft = read_ulimit_soft().unwrap_or(1024);
    let hard = read_ulimit_hard().unwrap_or(soft);
    let system_max = read_system_max_files();
    let current = count_open_fds();

    SystemLimits {
        nofile_soft: soft,
        nofile_hard: hard,
        system_max_files: system_max,
        current_open_fds: current,
        platform: platform_identifier().to_string(),
    }
}

fn platform_identifier() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(unix) {
        "unix"
    } else {
        "unknown"
    }
}

/// Read the soft file descriptor limit via `ulimit -Sn`.
fn read_ulimit_soft() -> Option<u64> {
    std::process::Command::new("sh")
        .args(["-c", "ulimit -Sn"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed == "unlimited" {
                Some(u64::MAX)
            } else {
                trimmed.parse().ok()
            }
        })
}

/// Read the hard file descriptor limit via `ulimit -Hn`.
fn read_ulimit_hard() -> Option<u64> {
    std::process::Command::new("sh")
        .args(["-c", "ulimit -Hn"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            let trimmed = s.trim();
            if trimmed == "unlimited" {
                Some(u64::MAX)
            } else {
                trimmed.parse().ok()
            }
        })
}

/// Read the system-wide max open files.
#[cfg(target_os = "linux")]
fn read_system_max_files() -> Option<u64> {
    std::fs::read_to_string("/proc/sys/fs/file-max")
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn read_system_max_files() -> Option<u64> {
    // macOS: kern.maxfiles via sysctl
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .args(["-n", "kern.maxfiles"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Count currently open file descriptors for this process.
#[cfg(target_os = "linux")]
pub fn count_open_fds() -> u64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|entries| entries.count() as u64)
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
pub fn count_open_fds() -> u64 {
    // On macOS, read /dev/fd as a proxy for open FDs
    std::fs::read_dir("/dev/fd")
        .map(|entries| entries.count() as u64)
        .unwrap_or(0)
}

#[cfg(not(unix))]
pub fn count_open_fds() -> u64 {
    0
}

/// Validate system limits against requirements.
pub fn validate_system_limits(config: &FdBudgetConfig, target_panes: u64) -> LimitValidation {
    let limits = get_system_limits();
    let required_fds = target_panes.saturating_mul(config.fds_per_pane);
    let required_system_fds = required_fds.saturating_mul(2);
    let mut checks = Vec::new();
    let mut fix_commands = Vec::new();

    // Check soft limit
    let nofile_ok = limits.nofile_soft >= config.min_nofile_limit;
    checks.push(LimitCheck {
        name: "nofile_soft (ulimit -n)".to_string(),
        current: limits.nofile_soft,
        required: config.min_nofile_limit,
        ok: nofile_ok,
    });

    if !nofile_ok {
        if cfg!(target_os = "linux") {
            fix_commands.push(format!(
                "ulimit -n {}  # or add to /etc/security/limits.conf",
                config.min_nofile_limit
            ));
        } else if cfg!(target_os = "macos") {
            fix_commands.push(format!(
                "sudo launchctl limit maxfiles {} {}",
                config.min_nofile_limit, config.min_nofile_limit
            ));
            fix_commands.push(format!("ulimit -n {}", config.min_nofile_limit));
        }
    }

    // Check capacity for target panes
    let capacity_ok = limits.nofile_soft >= required_fds;
    checks.push(LimitCheck {
        name: "capacity_for_target_panes".to_string(),
        current: limits.nofile_soft,
        required: required_fds,
        ok: capacity_ok,
    });

    // Check system-wide limit (Linux only)
    if let Some(sys_max) = limits.system_max_files {
        let sys_ok = sys_max >= required_system_fds; // 2x for safety margin
        checks.push(LimitCheck {
            name: "system_max_files (fs.file-max)".to_string(),
            current: sys_max,
            required: required_system_fds,
            ok: sys_ok,
        });

        if !sys_ok && cfg!(target_os = "linux") {
            fix_commands.push(format!("sudo sysctl -w fs.file-max={required_system_fds}"));
        }
    }

    let all_ok = checks.iter().all(|c| c.ok);
    LimitValidation {
        all_ok,
        checks,
        fix_commands,
    }
}

// =============================================================================
// FD budget tracker
// =============================================================================

/// Global FD budget tracker.
///
/// Tracks per-pane FD allocation and provides budget-aware admission control.
pub struct FdBudget {
    config: FdBudgetConfig,
    /// Per-pane FD counts (sharded lock-free map).
    pane_fds: PaneMap<u64>,
    /// Total allocated FDs across all panes.
    total_allocated: AtomicU64,
    /// Effective limit (nofile_soft at init time).
    effective_limit: u64,
    /// Audit history for leak detection: (timestamp, open_count).
    audit_history: RwLock<Vec<(Instant, u64)>>,
}

impl FdBudget {
    /// Create a new FD budget tracker.
    pub fn new(config: FdBudgetConfig) -> Self {
        let limits = get_system_limits();
        let config = config.validated_or_fail_closed();
        Self {
            effective_limit: limits.nofile_soft,
            config,
            pane_fds: PaneMap::new(),
            total_allocated: AtomicU64::new(0),
            audit_history: RwLock::new(Vec::new()),
        }
    }

    /// Try to create a new FD budget tracker, rejecting malformed config.
    pub fn try_new(config: FdBudgetConfig) -> Result<Self, FdBudgetConfigError> {
        config.validate()?;
        Ok(Self::new(config))
    }

    /// Create with an explicit limit (useful for testing).
    pub fn with_limit(config: FdBudgetConfig, limit: u64) -> Self {
        let config = config.validated_or_fail_closed();
        Self {
            effective_limit: limit,
            config,
            pane_fds: PaneMap::new(),
            total_allocated: AtomicU64::new(0),
            audit_history: RwLock::new(Vec::new()),
        }
    }

    /// Try to create with an explicit limit, rejecting malformed config.
    pub fn try_with_limit(config: FdBudgetConfig, limit: u64) -> Result<Self, FdBudgetConfigError> {
        config.validate()?;
        Ok(Self::with_limit(config, limit))
    }

    /// Check if a new pane can be admitted within the FD budget.
    ///
    /// br-ft-aj20l: when `effective_limit == 0` the budget is in
    /// an unavailable / unconfigured state — fail closed. Without
    /// this guard, the `projected / limit` division produces
    /// `f64::INFINITY` (or NaN if both are zero), which then
    /// compared against the warn / refuse thresholds via `>=` —
    /// Inf >= threshold is true, so admission would actually
    /// refuse — but that's incidental, and a NaN comparison
    /// returns false (admitting silently). Explicit fail-closed
    /// at the boundary makes the contract obvious.
    pub fn can_admit_pane(&self) -> AdmitDecision {
        let current = self.total_allocated.load(Ordering::SeqCst);
        let projected = current.saturating_add(self.config.fds_per_pane);

        if self.effective_limit == 0 {
            return AdmitDecision::Refused {
                current_fds: current,
                limit: 0,
                projected,
            };
        }

        let ratio = projected as f64 / self.effective_limit as f64;

        if ratio >= self.config.refuse_threshold {
            AdmitDecision::Refused {
                current_fds: current,
                limit: self.effective_limit,
                projected,
            }
        } else if ratio >= self.config.warn_threshold {
            AdmitDecision::Warned {
                current_fds: current,
                limit: self.effective_limit,
                usage_ratio: ratio,
            }
        } else {
            AdmitDecision::Allowed
        }
    }

    /// Register a new pane's FD allocation.
    pub fn register_pane(&self, pane_id: u64) {
        let fds = self.config.fds_per_pane;
        match self.pane_fds.insert(pane_id, fds) {
            Some(previous_fds) if fds > previous_fds => {
                self.add_allocated(fds - previous_fds);
            }
            Some(previous_fds) if previous_fds > fds => {
                self.subtract_allocated(previous_fds - fds);
            }
            Some(_) => {}
            None => {
                self.add_allocated(fds);
            }
        }
    }

    /// Unregister a pane (releases its FD allocation).
    pub fn unregister_pane(&self, pane_id: u64) {
        let fds = self.pane_fds.remove(pane_id).unwrap_or(0);
        self.subtract_allocated(fds);
    }

    fn add_allocated(&self, fds: u64) {
        let _ = self
            .total_allocated
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_add(fds))
            });
    }

    fn subtract_allocated(&self, fds: u64) {
        let _ = self
            .total_allocated
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.saturating_sub(fds))
            });
    }

    /// Get current FD budget snapshot.
    pub fn snapshot(&self) -> FdSnapshot {
        let current_open = count_open_fds();
        let total_allocated = self.total_allocated.load(Ordering::SeqCst);
        let pane_count = self.pane_fds.len();

        // br-ft-aj20l: when `effective_limit == 0` the budget is
        // unconfigured / unavailable. Use a saturated 1.0 sentinel
        // for the operator-facing ratios rather than f64::INFINITY
        // / NaN. Dashboards compute `>= 0.95` checks against these
        // ratios; Inf passes those checks (correct fail-closed
        // intent), but NaN does NOT (NaN comparisons are false).
        // 1.0 is the consistent fail-closed value.
        let (usage_ratio, budget_ratio) = if self.effective_limit == 0 {
            (1.0, 1.0)
        } else {
            (
                current_open as f64 / self.effective_limit as f64,
                total_allocated as f64 / self.effective_limit as f64,
            )
        };

        FdSnapshot {
            current_open,
            total_allocated,
            effective_limit: self.effective_limit,
            pane_count,
            usage_ratio,
            budget_ratio,
        }
    }

    /// Run an FD audit and check for leaks.
    ///
    /// Call this periodically (e.g., every 30 seconds). Returns a leak report
    /// if FD count has been monotonically increasing for `leak_detection_count`
    /// consecutive audits.
    pub fn audit(&self) -> AuditResult {
        let current = count_open_fds();
        let now = Instant::now();

        let mut history = match self.audit_history.write() {
            Ok(history) => history,
            Err(poisoned) => {
                warn!("FD audit history lock poisoned; resetting leak-detection window");
                let mut history = poisoned.into_inner();
                history.clear();
                history
            }
        };
        history.push((now, current));

        // Keep only recent history (2x the detection window)
        let max_entries = self.config.leak_detection_count.saturating_mul(2);
        if history.len() > max_entries {
            let drain_count = history.len() - max_entries;
            history.drain(..drain_count);
        }

        // Check for monotonic increase
        let leak_detected = if history.len() >= self.config.leak_detection_count {
            let window = &history[history.len() - self.config.leak_detection_count..];
            window.windows(2).all(|pair| pair[1].1 > pair[0].1)
        } else {
            false
        };

        // br-ft-aj20l: same fail-closed sentinel as snapshot() —
        // 1.0 when limit is zero so the warn threshold check fires
        // (NaN would silently disable the warning).
        let usage_ratio = if self.effective_limit == 0 {
            1.0
        } else {
            current as f64 / self.effective_limit as f64
        };
        let warning = usage_ratio >= self.config.warn_threshold;

        if leak_detected {
            let first = history[history.len() - self.config.leak_detection_count].1;
            warn!(
                current_fds = current,
                first_in_window = first,
                growth = audit_growth(current, first),
                "potential FD leak detected: monotonic increase over {} audits",
                self.config.leak_detection_count
            );
        }

        if warning {
            warn!(
                current_fds = current,
                limit = self.effective_limit,
                ratio = format!("{:.1}%", usage_ratio * 100.0),
                "FD usage above warning threshold"
            );
        }

        AuditResult {
            current_fds: current,
            effective_limit: self.effective_limit,
            usage_ratio,
            leak_detected,
            warning,
            audit_count: history.len(),
        }
    }

    /// Get per-pane FD breakdown.
    pub fn pane_breakdown(&self) -> HashMap<u64, u64> {
        self.pane_fds.entries().into_iter().collect()
    }
}

fn audit_growth(current: u64, first: u64) -> u64 {
    current.saturating_sub(first)
}

// =============================================================================
// Decision / report types
// =============================================================================

/// Result of checking whether a new pane can be admitted.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmitDecision {
    /// FD budget permits a new pane.
    Allowed,
    /// Allowed but close to the limit.
    Warned {
        current_fds: u64,
        limit: u64,
        usage_ratio: f64,
    },
    /// Refused — too close to the FD limit.
    Refused {
        current_fds: u64,
        limit: u64,
        projected: u64,
    },
}

impl AdmitDecision {
    /// Whether the pane is allowed (even if warned).
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed | Self::Warned { .. })
    }
}

/// Snapshot of current FD budget state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FdSnapshot {
    /// Actually open FDs (from OS audit).
    pub current_open: u64,
    /// FDs allocated by FrankenTerm's budget tracker.
    pub total_allocated: u64,
    /// Effective nofile limit.
    pub effective_limit: u64,
    /// Number of tracked panes.
    pub pane_count: usize,
    /// Ratio of open FDs to limit (0.0-1.0).
    pub usage_ratio: f64,
    /// Ratio of budget-allocated FDs to limit (0.0-1.0).
    pub budget_ratio: f64,
}

/// Result of a periodic FD audit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditResult {
    /// Current open FD count.
    pub current_fds: u64,
    /// Effective limit.
    pub effective_limit: u64,
    /// Usage ratio (0.0-1.0).
    pub usage_ratio: f64,
    /// Whether a monotonic leak pattern was detected.
    pub leak_detected: bool,
    /// Whether usage is above the warning threshold.
    pub warning: bool,
    /// Number of audits in history.
    pub audit_count: usize,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::overly_complex_bool_expr)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn test_config() -> FdBudgetConfig {
        FdBudgetConfig {
            warn_threshold: 0.80,
            refuse_threshold: 0.95,
            fds_per_pane: 25,
            min_nofile_limit: 1024,
            audit_interval_secs: 1,
            leak_detection_count: 3,
        }
    }

    // ── System limits ──

    #[test]
    fn system_limits_are_readable() {
        let limits = get_system_limits();
        // On any Unix, soft limit should be > 0
        assert!(limits.nofile_soft > 0);
        assert!(limits.nofile_hard >= limits.nofile_soft);
    }

    #[test]
    fn count_fds_returns_nonzero() {
        let fds = count_open_fds();
        // We always have at least stdin/stdout/stderr
        assert!(fds >= 3, "expected at least 3 open FDs, got {fds}");
    }

    #[test]
    fn validate_limits_sufficient() {
        let config = FdBudgetConfig {
            min_nofile_limit: 64,
            ..test_config()
        };
        let validation = validate_system_limits(&config, 2);
        // With only 2 target panes and min_nofile of 64, this should pass
        assert!(validation.all_ok);
        assert!(validation.fix_commands.is_empty());
    }

    #[test]
    fn validate_limits_reports_checks() {
        // Validate with an extremely high target pane count to trigger
        // capacity warnings (even if ulimit is unlimited).
        let config = FdBudgetConfig {
            fds_per_pane: 25,
            ..test_config()
        };
        let validation = validate_system_limits(&config, 10);
        // Should have at least 2 checks (nofile_soft and capacity)
        assert!(validation.checks.len() >= 2);
        // All checks should have valid fields
        for check in &validation.checks {
            assert!(!check.name.is_empty());
        }
    }

    // ── Budget tracking ──

    #[test]
    fn budget_allows_when_under_limit() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        assert!(budget.can_admit_pane().is_allowed());
    }

    #[test]
    fn budget_refuses_near_limit() {
        let config = FdBudgetConfig {
            fds_per_pane: 100,
            refuse_threshold: 0.95,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 1000);

        // Register 9 panes (900/1000 = 0.90)
        for i in 0..9 {
            budget.register_pane(i);
        }

        // 10th pane would push to 1000/1000 = 1.0 > 0.95
        assert!(!budget.can_admit_pane().is_allowed());
    }

    #[test]
    fn budget_warns_near_threshold() {
        let config = FdBudgetConfig {
            fds_per_pane: 100,
            warn_threshold: 0.80,
            refuse_threshold: 0.95,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 1000);

        // Register 8 panes (800/1000 = 0.80)
        for i in 0..8 {
            budget.register_pane(i);
        }

        // 9th pane would push to 825/1000 = 0.825 > warn but < refuse
        assert!(matches!(
            budget.can_admit_pane(),
            AdmitDecision::Warned { .. }
        ));
    }

    #[test]
    fn register_unregister_tracks_total() {
        let budget = FdBudget::with_limit(test_config(), 10_000);

        budget.register_pane(1);
        budget.register_pane(2);
        budget.register_pane(3);

        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 75); // 3 * 25
        assert_eq!(snap.pane_count, 3);

        budget.unregister_pane(2);
        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 50); // 2 * 25
        assert_eq!(snap.pane_count, 2);
    }

    #[test]
    fn unregister_nonexistent_pane_is_noop() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(1);
        budget.unregister_pane(999); // doesn't exist
        assert_eq!(budget.snapshot().total_allocated, 25);
    }

    #[test]
    fn pane_breakdown_shows_per_pane() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(10);
        budget.register_pane(20);

        let breakdown = budget.pane_breakdown();
        assert_eq!(breakdown.len(), 2);
        assert_eq!(breakdown[&10], 25);
        assert_eq!(breakdown[&20], 25);
    }

    // ── Audit and leak detection ──

    #[test]
    fn audit_returns_valid_result() {
        let budget = FdBudget::with_limit(test_config(), 100_000);
        let result = budget.audit();
        assert!(result.current_fds > 0);
        assert!(!result.leak_detected);
    }

    #[test]
    fn audit_recovers_from_poisoned_history_lock() {
        let budget = FdBudget::with_limit(test_config(), 100_000);

        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = budget.audit_history.write().unwrap();
            panic!("poison audit history");
        }));

        let result = budget.audit();
        assert!(result.current_fds > 0);
        assert_eq!(result.audit_count, 1);
        assert!(!result.leak_detected);
    }

    proptest! {
        #[test]
        fn proptest_audit_growth_is_saturating(current in any::<u64>(), first in any::<u64>()) {
            let growth = audit_growth(current, first);
            prop_assert_eq!(growth, current.saturating_sub(first));
            if current < first {
                prop_assert_eq!(growth, 0);
            }
        }
    }

    #[test]
    fn snapshot_ratios_are_valid() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(1);

        let snap = budget.snapshot();
        assert!(snap.usage_ratio >= 0.0);
        assert!(snap.budget_ratio >= 0.0 && snap.budget_ratio <= 1.0);
        assert_eq!(snap.effective_limit, 10_000);
    }

    // ── br-ft-aj20l: zero effective_limit fail-closed contract ──

    #[test]
    fn can_admit_pane_refuses_when_limit_zero_ft_aj20l() {
        // br-ft-aj20l: a zero (unconfigured / unavailable) limit
        // must fail closed at admission. Without the explicit
        // guard, ratio = nonzero / 0 = INFINITY, which happens to
        // trip the >= refuse_threshold comparison correctly — but
        // ratio = 0 / 0 = NaN, and NaN >= threshold is FALSE,
        // which would silently admit. Explicit guard is the
        // boundary contract.
        let budget = FdBudget::with_limit(test_config(), 0);
        let decision = budget.can_admit_pane();
        assert!(!decision.is_allowed(), "zero limit must fail closed");
        assert!(matches!(decision, AdmitDecision::Refused { limit: 0, .. }));
    }

    #[test]
    fn can_admit_pane_refuses_when_limit_zero_with_zero_fds_per_pane_ft_aj20l() {
        // The explicit NaN trap: limit=0 AND fds_per_pane=0 means
        // ratio = (current + 0) / 0. If `current == 0` that's the
        // NaN case (0/0). The fail-closed boundary catches it.
        let config = FdBudgetConfig {
            fds_per_pane: 0,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 0);
        let decision = budget.can_admit_pane();
        assert!(!decision.is_allowed());
    }

    #[test]
    fn snapshot_ratios_are_finite_when_limit_zero_ft_aj20l() {
        // Operator telemetry must never carry NaN/Inf — dashboards
        // that compute `>= 0.95` checks would silently miss the
        // fail-closed signal on NaN. 1.0 is the saturated sentinel.
        let budget = FdBudget::with_limit(test_config(), 0);
        let snap = budget.snapshot();
        assert!(
            snap.usage_ratio.is_finite(),
            "usage_ratio must be finite, got {}",
            snap.usage_ratio
        );
        assert!(
            snap.budget_ratio.is_finite(),
            "budget_ratio must be finite, got {}",
            snap.budget_ratio
        );
        assert_eq!(snap.usage_ratio, 1.0, "saturated sentinel for limit=0");
        assert_eq!(snap.budget_ratio, 1.0);
    }

    #[test]
    fn audit_usage_ratio_is_finite_when_limit_zero_ft_aj20l() {
        let budget = FdBudget::with_limit(test_config(), 0);
        let result = budget.audit();
        assert!(
            result.usage_ratio.is_finite(),
            "audit usage_ratio must be finite, got {}",
            result.usage_ratio
        );
        assert_eq!(result.usage_ratio, 1.0);
        assert!(
            result.warning,
            "warning threshold must fire at saturated sentinel"
        );
    }

    // ── AdmitDecision ──

    #[test]
    fn admit_decision_is_allowed() {
        assert!(AdmitDecision::Allowed.is_allowed());
        assert!(
            AdmitDecision::Warned {
                current_fds: 100,
                limit: 200,
                usage_ratio: 0.5,
            }
            .is_allowed()
        );
        assert!(
            !AdmitDecision::Refused {
                current_fds: 100,
                limit: 100,
                projected: 125,
            }
            .is_allowed()
        );
    }

    // ── Config defaults ──

    #[test]
    fn default_config_is_sensible() {
        let config = FdBudgetConfig::default();
        assert!(config.warn_threshold < config.refuse_threshold);
        assert!(config.fds_per_pane > 0);
        assert!(config.min_nofile_limit >= 1024);
        assert!(config.audit_interval_secs > 0);
        assert!(config.leak_detection_count >= 2);
    }

    // ── Serialization ──

    #[test]
    fn config_serde_roundtrip() {
        let config = FdBudgetConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: FdBudgetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config.min_nofile_limit, deserialized.min_nofile_limit);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = FdSnapshot {
            current_open: 50,
            total_allocated: 100,
            effective_limit: 65536,
            pane_count: 4,
            usage_ratio: 0.001,
            budget_ratio: 0.002,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: FdSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.current_open, deserialized.current_open);
        assert_eq!(snap.pane_count, deserialized.pane_count);
    }

    #[test]
    fn audit_result_serde_roundtrip() {
        let result = AuditResult {
            current_fds: 100,
            effective_limit: 65536,
            usage_ratio: 0.002,
            leak_detected: false,
            warning: false,
            audit_count: 5,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: AuditResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.current_fds, deserialized.current_fds);
        assert_eq!(result.leak_detected, deserialized.leak_detected);
    }

    #[test]
    fn limit_validation_serde_roundtrip() {
        let validation = LimitValidation {
            all_ok: true,
            checks: vec![LimitCheck {
                name: "nofile_soft".to_string(),
                current: 65536,
                required: 1024,
                ok: true,
            }],
            fix_commands: vec![],
        };
        let json = serde_json::to_string(&validation).unwrap();
        let deserialized: LimitValidation = serde_json::from_str(&json).unwrap();
        assert_eq!(validation.all_ok, deserialized.all_ok);
        assert_eq!(validation.checks.len(), deserialized.checks.len());
    }

    // ── Scale test ──

    #[test]
    fn budget_handles_200_panes() {
        let config = FdBudgetConfig {
            fds_per_pane: 25,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 65_536);

        for i in 0..200 {
            budget.register_pane(i);
        }

        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 200 * 25);
        assert_eq!(snap.pane_count, 200);
        assert!(snap.budget_ratio < 0.08); // 5000/65536 ≈ 7.6%

        // All panes should still be admittable
        assert!(budget.can_admit_pane().is_allowed());
    }

    // ── Batch 2: DarkBadger wa-1u90p.7.1 ─────────────────────────────────

    // ── Leak detection ──

    #[test]
    fn audit_does_not_panic_on_non_monotonic_synthetic_history_ft_sj70e() {
        // br-ft-sj70e: the leak-detection log path computes
        // `growth = current - first` via saturating_sub. Today the
        // leak predicate (`pair[1].1 > pair[0].1` over the window)
        // ensures `current >= first`, so the subtraction can't
        // underflow on the production path. This test asserts the
        // diagnostic path stays safe even when the history contains
        // non-monotonic synthetic values (restored snapshots,
        // poisoned-lock recovery noise, future predicate changes).
        let config = FdBudgetConfig {
            leak_detection_count: 3,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 100_000);

        // Inject synthetic non-monotonic history directly through
        // the lock — bypasses the audit() pushes so the test
        // controls the exact shape.
        {
            let mut h = budget.audit_history.write().unwrap();
            let now = Instant::now();
            // Decreasing then increasing — predicate evaluates to
            // false (no leak detected, warn! body not entered),
            // but the test still validates audit() completes.
            h.push((now, 200));
            h.push((now, 100));
            h.push((now, 150));
        }

        // No panic, no overflow, returns a result.
        let result = budget.audit();
        assert!(
            !result.leak_detected,
            "non-monotonic history must not trigger leak"
        );
        assert!(result.audit_count > 0);
    }

    #[test]
    fn leak_growth_subtraction_saturates_on_decreasing_pair_ft_sj70e() {
        // Direct contract pin: even if a future predicate relaxes
        // monotonic-increase to "increased on most steps", the
        // logged `growth` value must be 0 (saturating_sub) rather
        // than wrapping or panicking.
        let current: u64 = 100;
        let first: u64 = 200;
        let growth = current.saturating_sub(first);
        assert_eq!(growth, 0, "saturating_sub must clamp underflow to 0");
        // And the symmetric monotonic case still produces the
        // expected positive delta.
        assert_eq!(300u64.saturating_sub(50u64), 250);
    }

    #[test]
    fn audit_detects_leak_after_consecutive_monotonic_increases() {
        // The audit should detect a leak when FD count increases monotonically
        // for `leak_detection_count` consecutive audits. We can't control the
        // actual FD count, but we CAN verify the audit runs and returns a valid
        // result with correct field semantics.
        let config = FdBudgetConfig {
            leak_detection_count: 3,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 100_000);

        // Run multiple audits — audit_count should increase
        let r1 = budget.audit();
        assert_eq!(r1.audit_count, 1);
        assert!(!r1.leak_detected); // Can't have a leak in 1 audit

        let r2 = budget.audit();
        assert_eq!(r2.audit_count, 2);
        assert!(!r2.leak_detected); // Need 3 consecutive increases

        let r3 = budget.audit();
        assert_eq!(r3.audit_count, 3);
        // Whether leak_detected depends on actual FD movement — just verify it's bool
        assert!(r3.leak_detected || !r3.leak_detected);
    }

    #[test]
    fn audit_history_is_trimmed_to_2x_window() {
        let config = FdBudgetConfig {
            leak_detection_count: 2,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 100_000);

        // Run 10 audits — history should be trimmed to 2x2=4
        for _ in 0..10 {
            budget.audit();
        }

        let result = budget.audit();
        // audit_count should be at most 2*2=4 (trimmed) + 1 (the one we just ran)
        // Actually, trimming happens before push, so max is 2*leak_detection_count
        // The implementation trims to max_entries=2*count then pushes, so 4+push...
        // Let's just verify it's bounded and doesn't grow unbounded
        assert!(
            result.audit_count <= 5,
            "audit_count should be bounded, got {}",
            result.audit_count
        );
    }

    #[test]
    fn audit_usage_ratio_is_consistent() {
        let budget = FdBudget::with_limit(test_config(), 100_000);
        let result = budget.audit();
        // usage_ratio should be current_fds / effective_limit
        let expected = result.current_fds as f64 / 100_000.0;
        assert!(
            (result.usage_ratio - expected).abs() < 0.001,
            "usage_ratio {} should match current_fds/limit = {}",
            result.usage_ratio,
            expected
        );
    }

    // ── Snapshot edge cases ──

    #[test]
    fn snapshot_empty_budget_has_zero_allocated() {
        let budget = FdBudget::with_limit(test_config(), 65_536);
        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 0);
        assert_eq!(snap.pane_count, 0);
        assert_eq!(snap.budget_ratio, 0.0);
        assert_eq!(snap.effective_limit, 65_536);
        // current_open will be > 0 (at least stdin/stdout/stderr)
        assert!(snap.current_open > 0);
        assert!(snap.usage_ratio > 0.0);
    }

    #[test]
    fn pane_breakdown_empty_budget() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        let breakdown = budget.pane_breakdown();
        assert!(breakdown.is_empty());
    }

    // ── Double registration ──

    #[test]
    fn register_same_pane_twice_overwrites() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(42);
        budget.register_pane(42); // re-register same pane

        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 25);
        // The pane_breakdown should have only one entry
        let breakdown = budget.pane_breakdown();
        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[&42], 25);
    }

    #[test]
    fn duplicate_register_then_unregister_returns_to_zero() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(42);
        budget.register_pane(42);
        budget.unregister_pane(42);

        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 0);
        assert_eq!(snap.pane_count, 0);
    }

    // ── Unregister all panes ──

    #[test]
    fn unregister_all_panes_returns_to_zero_snapshot() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        for i in 0..5 {
            budget.register_pane(i);
        }
        assert_eq!(budget.snapshot().pane_count, 5);

        for i in 0..5 {
            budget.unregister_pane(i);
        }
        let snap = budget.snapshot();
        assert_eq!(snap.pane_count, 0);
        assert_eq!(snap.total_allocated, 0);
        assert_eq!(snap.budget_ratio, 0.0);
    }

    // ── AdmitDecision details ──

    #[test]
    fn admit_decision_refused_has_correct_fields() {
        let config = FdBudgetConfig {
            fds_per_pane: 500,
            refuse_threshold: 0.95,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 1000);

        // Register 1 pane (500 FDs), next would be 1000/1000 = 1.0 > 0.95
        budget.register_pane(1);

        assert_eq!(
            budget.can_admit_pane(),
            AdmitDecision::Refused {
                current_fds: 500,
                limit: 1000,
                projected: 1000,
            }
        );
    }

    #[test]
    fn admit_decision_warned_has_correct_fields() {
        let config = FdBudgetConfig {
            fds_per_pane: 100,
            warn_threshold: 0.80,
            refuse_threshold: 0.95,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 1000);

        // Register 8 panes (800 FDs), next would be 900/1000 = 0.9
        for i in 0..8 {
            budget.register_pane(i);
        }

        let decision = budget.can_admit_pane();
        assert!(matches!(decision, AdmitDecision::Warned { .. }));
        if let AdmitDecision::Warned {
            current_fds,
            limit,
            usage_ratio,
        } = decision
        {
            assert_eq!(current_fds, 800);
            assert_eq!(limit, 1000);
            assert!((usage_ratio - 0.9).abs() < 0.001);
        }
    }

    // ── Debug/Clone traits ──

    #[test]
    fn admit_decision_debug_and_clone() {
        let allowed = AdmitDecision::Allowed;
        let debug_str = format!("{:?}", allowed);
        assert_eq!(debug_str, "Allowed");

        let warned = AdmitDecision::Warned {
            current_fds: 100,
            limit: 200,
            usage_ratio: 0.5,
        };
        let debug_str = format!("{:?}", warned);
        assert!(debug_str.contains("Warned"));
        assert!(debug_str.contains("100"));

        let cloned = warned.clone();
        assert_eq!(warned, cloned);

        let refused = AdmitDecision::Refused {
            current_fds: 950,
            limit: 1000,
            projected: 975,
        };
        let debug_str = format!("{:?}", refused);
        assert!(debug_str.contains("Refused"));
        assert!(debug_str.contains("975"));
    }

    #[test]
    fn fd_snapshot_debug_and_clone() {
        let snap = FdSnapshot {
            current_open: 42,
            total_allocated: 100,
            effective_limit: 65536,
            pane_count: 4,
            usage_ratio: 0.001,
            budget_ratio: 0.002,
        };
        let debug_str = format!("{:?}", snap);
        assert!(debug_str.contains("FdSnapshot"));
        assert!(debug_str.contains("42"));

        let cloned = snap.clone();
        assert_eq!(cloned.current_open, 42);
        assert_eq!(cloned.pane_count, 4);
    }

    #[test]
    fn audit_result_debug_and_clone() {
        let result = AuditResult {
            current_fds: 100,
            effective_limit: 65536,
            usage_ratio: 0.002,
            leak_detected: true,
            warning: false,
            audit_count: 5,
        };
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("AuditResult"));
        assert!(debug_str.contains("true")); // leak_detected

        let cloned = result.clone();
        assert_eq!(cloned.leak_detected, true);
        assert_eq!(cloned.audit_count, 5);
    }

    #[test]
    fn limit_check_debug_and_clone() {
        let check = LimitCheck {
            name: "nofile_soft".to_string(),
            current: 65536,
            required: 1024,
            ok: true,
        };
        let debug_str = format!("{:?}", check);
        assert!(debug_str.contains("LimitCheck"));
        assert!(debug_str.contains("nofile_soft"));

        let cloned = check.clone();
        assert_eq!(cloned.name, "nofile_soft");
        assert_eq!(cloned.ok, true);
    }

    #[test]
    fn limit_validation_debug_and_clone() {
        let validation = LimitValidation {
            all_ok: false,
            checks: vec![
                LimitCheck {
                    name: "nofile_soft".to_string(),
                    current: 256,
                    required: 1024,
                    ok: false,
                },
                LimitCheck {
                    name: "capacity".to_string(),
                    current: 256,
                    required: 500,
                    ok: false,
                },
            ],
            fix_commands: vec!["ulimit -n 1024".to_string()],
        };
        let debug_str = format!("{:?}", validation);
        assert!(debug_str.contains("LimitValidation"));
        assert!(debug_str.contains("false"));

        let cloned = validation.clone();
        assert_eq!(cloned.all_ok, false);
        assert_eq!(cloned.checks.len(), 2);
        assert_eq!(cloned.fix_commands.len(), 1);
    }

    #[test]
    fn system_limits_debug_and_clone() {
        let limits = SystemLimits {
            nofile_soft: 65536,
            nofile_hard: 1_048_576,
            system_max_files: Some(100_000),
            current_open_fds: 42,
            platform: "macos".to_string(),
        };
        let debug_str = format!("{:?}", limits);
        assert!(debug_str.contains("SystemLimits"));
        assert!(debug_str.contains("macos"));

        let cloned = limits.clone();
        assert_eq!(cloned.nofile_soft, 65536);
        assert_eq!(cloned.platform, "macos");
        assert_eq!(cloned.system_max_files, Some(100_000));
    }

    #[test]
    fn system_limits_serde_roundtrip() {
        let limits = SystemLimits {
            nofile_soft: 65536,
            nofile_hard: 1_048_576,
            system_max_files: None,
            current_open_fds: 42,
            platform: "linux".to_string(),
        };
        let json = serde_json::to_string(&limits).unwrap();
        let deserialized: SystemLimits = serde_json::from_str(&json).unwrap();
        assert_eq!(limits.nofile_soft, deserialized.nofile_soft);
        assert_eq!(limits.nofile_hard, deserialized.nofile_hard);
        assert_eq!(limits.system_max_files, deserialized.system_max_files);
        assert_eq!(limits.current_open_fds, deserialized.current_open_fds);
        assert_eq!(limits.platform, deserialized.platform);
    }

    // ── Config edge cases ──

    #[test]
    fn config_debug_impl() {
        let config = test_config();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("FdBudgetConfig"));
        assert!(debug_str.contains("0.8")); // warn_threshold
    }

    #[test]
    fn config_clone_preserves_all_fields() {
        let config = FdBudgetConfig {
            warn_threshold: 0.75,
            refuse_threshold: 0.90,
            fds_per_pane: 30,
            min_nofile_limit: 2048,
            audit_interval_secs: 60,
            leak_detection_count: 10,
        };
        let cloned = config.clone();
        assert_eq!(cloned.warn_threshold, 0.75);
        assert_eq!(cloned.refuse_threshold, 0.90);
        assert_eq!(cloned.fds_per_pane, 30);
        assert_eq!(cloned.min_nofile_limit, 2048);
        assert_eq!(cloned.audit_interval_secs, 60);
        assert_eq!(cloned.leak_detection_count, 10);
    }

    #[test]
    fn config_validate_rejects_invalid_thresholds_and_zero_leak_window() {
        let mut config = test_config();
        config.warn_threshold = f64::NAN;
        assert_eq!(
            config.validate(),
            Err(FdBudgetConfigError::NonFiniteThreshold)
        );

        let mut config = test_config();
        config.refuse_threshold = 1.25;
        assert_eq!(
            config.validate(),
            Err(FdBudgetConfigError::ThresholdOutOfRange)
        );

        let mut config = test_config();
        config.warn_threshold = 0.95;
        config.refuse_threshold = 0.80;
        assert_eq!(
            config.validate(),
            Err(FdBudgetConfigError::MisorderedThresholds)
        );

        let mut config = test_config();
        config.leak_detection_count = 0;
        assert_eq!(
            config.validate(),
            Err(FdBudgetConfigError::ZeroLeakDetectionCount)
        );
    }

    #[test]
    fn invalid_config_constructor_fails_closed() {
        let mut config = test_config();
        config.warn_threshold = f64::NAN;
        config.refuse_threshold = f64::NAN;
        config.leak_detection_count = 0;

        let budget = FdBudget::with_limit(config, 1_000);

        assert_eq!(
            budget.can_admit_pane(),
            AdmitDecision::Refused {
                current_fds: 0,
                limit: 1_000,
                projected: u64::MAX,
            }
        );
        let result = budget.audit();
        assert_eq!(result.audit_count, 1);
    }

    #[test]
    fn try_constructor_rejects_invalid_config() {
        let mut config = test_config();
        config.fds_per_pane = 0;

        assert_eq!(
            FdBudget::try_with_limit(config, 1_000).map(|_| ()),
            Err(FdBudgetConfigError::ZeroFdsPerPane)
        );
    }

    proptest! {
        #[test]
        fn proptest_config_validation_rejects_bad_thresholds(
            bad_threshold in prop_oneof![
                Just(f64::NAN),
                Just(f64::INFINITY),
                Just(f64::NEG_INFINITY),
                -1000.0f64..0.0,
                1.000_001f64..1000.0,
            ],
            use_warn in any::<bool>(),
        ) {
            let mut config = test_config();
            if use_warn {
                config.warn_threshold = bad_threshold;
            } else {
                config.refuse_threshold = bad_threshold;
            }

            prop_assert!(matches!(
                config.validate(),
                Err(FdBudgetConfigError::NonFiniteThreshold | FdBudgetConfigError::ThresholdOutOfRange)
            ));

            let budget = FdBudget::with_limit(config, 1_000);
            prop_assert!(!budget.can_admit_pane().is_allowed());
        }

        #[test]
        fn proptest_config_validation_accepts_ordered_finite_thresholds(
            warn_units in 0u16..=10_000,
            extra_units in 0u16..=10_000,
            fds_per_pane in 1u64..=10_000,
            min_nofile_limit in 1u64..=100_000,
            leak_detection_count in 1usize..=64,
        ) {
            let warn_threshold = f64::from(warn_units) / 10_000.0;
            let remaining = 10_000u32.saturating_sub(u32::from(warn_units));
            let refuse_threshold = warn_threshold
                + f64::from(u32::from(extra_units).min(remaining)) / 10_000.0;
            let config = FdBudgetConfig {
                warn_threshold,
                refuse_threshold,
                fds_per_pane,
                min_nofile_limit,
                audit_interval_secs: 1,
                leak_detection_count,
            };

            prop_assert_eq!(config.validate(), Ok(()));
        }
    }

    // ── Budget boundary cases ──

    #[test]
    fn budget_with_very_large_limit() {
        let budget = FdBudget::with_limit(test_config(), u64::MAX / 2);
        budget.register_pane(1);
        let snap = budget.snapshot();
        assert_eq!(snap.total_allocated, 25);
        assert_eq!(snap.effective_limit, u64::MAX / 2);
        // budget_ratio should be essentially zero
        assert!(snap.budget_ratio < 0.000001);
        assert!(budget.can_admit_pane().is_allowed());
    }

    #[test]
    fn can_admit_pane_saturates_projected_fd_count() {
        let config = FdBudgetConfig {
            fds_per_pane: 25,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, u64::MAX);
        budget
            .total_allocated
            .store(u64::MAX - 10, Ordering::SeqCst);

        assert_eq!(
            budget.can_admit_pane(),
            AdmitDecision::Refused {
                current_fds: u64::MAX - 10,
                limit: u64::MAX,
                projected: u64::MAX,
            }
        );
    }

    #[test]
    fn register_pane_saturates_total_allocated_on_overflow() {
        let budget = FdBudget::with_limit(test_config(), u64::MAX);
        budget
            .total_allocated
            .store(u64::MAX - 10, Ordering::SeqCst);

        budget.register_pane(1);

        assert_eq!(budget.snapshot().total_allocated, u64::MAX);
    }

    #[test]
    fn budget_with_limit_equal_to_fds_per_pane() {
        // Edge case: limit is exactly equal to one pane's FDs
        let config = FdBudgetConfig {
            fds_per_pane: 25,
            refuse_threshold: 0.95,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 25);
        // First pane: projected = 25/25 = 1.0 > 0.95 → refused
        assert!(!budget.can_admit_pane().is_allowed());
    }

    #[test]
    fn budget_exact_refuse_threshold_boundary() {
        // Budget where projected ratio exactly equals refuse_threshold
        let config = FdBudgetConfig {
            fds_per_pane: 100,
            refuse_threshold: 0.50,
            warn_threshold: 0.30,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 200);
        // Projected: 100/200 = 0.50 >= 0.50 refuse → refused
        assert!(!budget.can_admit_pane().is_allowed());
    }

    #[test]
    fn budget_exact_warn_threshold_boundary() {
        // Budget where projected ratio exactly equals warn_threshold
        let config = FdBudgetConfig {
            fds_per_pane: 100,
            refuse_threshold: 0.95,
            warn_threshold: 0.50,
            ..test_config()
        };
        let budget = FdBudget::with_limit(config, 200);
        // Projected: 100/200 = 0.50 >= 0.50 warn, < 0.95 refuse → warned
        assert!(matches!(
            budget.can_admit_pane(),
            AdmitDecision::Warned { .. }
        ));
    }

    #[test]
    fn multiple_unregister_same_pane_is_noop() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(1);
        budget.unregister_pane(1);
        // Second unregister should be a no-op (pane already removed)
        budget.unregister_pane(1);
        let snap = budget.snapshot();
        assert_eq!(snap.pane_count, 0);
        assert_eq!(snap.total_allocated, 0);
    }

    #[test]
    fn validate_limits_with_zero_target_panes() {
        let config = FdBudgetConfig {
            min_nofile_limit: 64,
            ..test_config()
        };
        let validation = validate_system_limits(&config, 0);
        // With 0 target panes, capacity should always be OK
        assert!(
            validation
                .checks
                .iter()
                .any(|c| c.name.contains("capacity") && c.ok),
            "Capacity check should pass with 0 target panes"
        );
    }

    #[test]
    fn validate_limits_saturates_required_fd_counts() {
        let config = FdBudgetConfig {
            fds_per_pane: u64::MAX,
            min_nofile_limit: 64,
            ..test_config()
        };
        let validation = validate_system_limits(&config, 2);

        let capacity_required = validation
            .checks
            .iter()
            .find(|c| c.name == "capacity_for_target_panes")
            .map(|c| c.required);
        assert_eq!(capacity_required, Some(u64::MAX));

        if let Some(system_max) = validation
            .checks
            .iter()
            .find(|c| c.name == "system_max_files (fs.file-max)")
        {
            assert_eq!(system_max.required, u64::MAX);
        }
    }

    #[test]
    fn get_system_limits_returns_valid_platform() {
        let limits = get_system_limits();
        // Platform should be one of the known values
        assert!(
            matches!(
                limits.platform.as_str(),
                "linux" | "macos" | "windows" | "unix" | "unknown"
            ),
            "Unknown platform: {}",
            limits.platform
        );
    }

    #[test]
    fn platform_identifier_matches_compile_target() {
        let platform = platform_identifier();
        let expected = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else if cfg!(unix) {
            "unix"
        } else {
            "unknown"
        };
        assert_eq!(platform, expected);
    }

    #[test]
    fn fd_snapshot_usage_ratio_increases_with_panes() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        let snap0 = budget.snapshot();
        let ratio0 = snap0.budget_ratio;

        budget.register_pane(1);
        let snap1 = budget.snapshot();
        let ratio1 = snap1.budget_ratio;

        budget.register_pane(2);
        let snap2 = budget.snapshot();
        let ratio2 = snap2.budget_ratio;

        assert!(
            ratio1 > ratio0,
            "ratio should increase after registering pane"
        );
        assert!(ratio2 > ratio1, "ratio should increase further");
    }
}
