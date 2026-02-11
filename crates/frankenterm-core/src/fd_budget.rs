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

// =============================================================================
// System limits
// =============================================================================

/// System resource limits relevant to FD management.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitValidation {
    /// Whether all limits are sufficient.
    pub all_ok: bool,
    /// Individual check results.
    pub checks: Vec<LimitCheck>,
    /// Platform-specific fix commands (if any limits are insufficient).
    pub fix_commands: Vec<String>,
}

/// A single limit check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
        platform: if cfg!(target_os = "linux") {
            "linux".to_string()
        } else if cfg!(target_os = "macos") {
            "macos".to_string()
        } else {
            "unix".to_string()
        },
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
    let required_fds = target_panes * config.fds_per_pane;
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
        let sys_ok = sys_max >= required_fds * 2; // 2x for safety margin
        checks.push(LimitCheck {
            name: "system_max_files (fs.file-max)".to_string(),
            current: sys_max,
            required: required_fds * 2,
            ok: sys_ok,
        });

        if !sys_ok && cfg!(target_os = "linux") {
            fix_commands.push(format!("sudo sysctl -w fs.file-max={}", required_fds * 2));
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
    /// Per-pane FD counts.
    pane_fds: RwLock<HashMap<u64, u64>>,
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
        Self {
            effective_limit: limits.nofile_soft,
            config,
            pane_fds: RwLock::new(HashMap::new()),
            total_allocated: AtomicU64::new(0),
            audit_history: RwLock::new(Vec::new()),
        }
    }

    /// Create with an explicit limit (useful for testing).
    pub fn with_limit(config: FdBudgetConfig, limit: u64) -> Self {
        Self {
            effective_limit: limit,
            config,
            pane_fds: RwLock::new(HashMap::new()),
            total_allocated: AtomicU64::new(0),
            audit_history: RwLock::new(Vec::new()),
        }
    }

    /// Check if a new pane can be admitted within the FD budget.
    pub fn can_admit_pane(&self) -> AdmitDecision {
        let current = self.total_allocated.load(Ordering::SeqCst);
        let projected = current + self.config.fds_per_pane;
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
        {
            let mut map = self.pane_fds.write().expect("lock poisoned");
            map.insert(pane_id, fds);
        }
        self.total_allocated.fetch_add(fds, Ordering::SeqCst);
    }

    /// Unregister a pane (releases its FD allocation).
    pub fn unregister_pane(&self, pane_id: u64) {
        let fds = {
            let mut map = self.pane_fds.write().expect("lock poisoned");
            map.remove(&pane_id).unwrap_or(0)
        };
        self.total_allocated.fetch_sub(fds, Ordering::SeqCst);
    }

    /// Get current FD budget snapshot.
    pub fn snapshot(&self) -> FdSnapshot {
        let current_open = count_open_fds();
        let total_allocated = self.total_allocated.load(Ordering::SeqCst);
        let pane_count = self.pane_fds.read().expect("lock poisoned").len();

        FdSnapshot {
            current_open,
            total_allocated,
            effective_limit: self.effective_limit,
            pane_count,
            usage_ratio: current_open as f64 / self.effective_limit as f64,
            budget_ratio: total_allocated as f64 / self.effective_limit as f64,
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

        let mut history = self.audit_history.write().expect("lock poisoned");
        history.push((now, current));

        // Keep only recent history (2x the detection window)
        let max_entries = self.config.leak_detection_count * 2;
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

        let usage_ratio = current as f64 / self.effective_limit as f64;
        let warning = usage_ratio >= self.config.warn_threshold;

        if leak_detected {
            let first = history[history.len() - self.config.leak_detection_count].1;
            warn!(
                current_fds = current,
                first_in_window = first,
                growth = current - first,
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
        self.pane_fds.read().expect("lock poisoned").clone()
    }
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
mod tests {
    use super::*;

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
        match budget.can_admit_pane() {
            AdmitDecision::Warned { .. } => {} // expected
            other => panic!("expected Warned, got {:?}", other),
        }
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
    fn snapshot_ratios_are_valid() {
        let budget = FdBudget::with_limit(test_config(), 10_000);
        budget.register_pane(1);

        let snap = budget.snapshot();
        assert!(snap.usage_ratio >= 0.0);
        assert!(snap.budget_ratio >= 0.0 && snap.budget_ratio <= 1.0);
        assert_eq!(snap.effective_limit, 10_000);
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
}
