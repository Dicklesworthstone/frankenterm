//! Watchdog and heartbeat system for deadlock detection and auto-recovery.
//!
//! Each runtime subsystem (discovery, capture, persistence, maintenance)
//! updates a heartbeat timestamp on every loop iteration.  A background
//! monitor task periodically checks these timestamps and logs warnings
//! when a subsystem appears stalled.
//!
//! # Integration
//!
//! ```text
//! ObservationRuntime
//!   ├── discovery_task ──► heartbeats.record_discovery()
//!   ├── capture_task   ──► heartbeats.record_capture()
//!   ├── persistence    ──► heartbeats.record_persistence()
//!   ├── maintenance    ──► heartbeats.record_maintenance()
//!   └── watchdog_task  ──► heartbeats.check_health()
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Per-component heartbeat timestamps (epoch milliseconds).
///
/// Each subsystem calls the corresponding `record_*` method on every
/// iteration of its main loop.  The watchdog monitor reads these to
/// determine whether a component is stalled.
#[derive(Debug)]
pub struct HeartbeatRegistry {
    discovery: AtomicU64,
    capture: AtomicU64,
    persistence: AtomicU64,
    maintenance: AtomicU64,
    /// Epoch ms when the registry was created (for grace period).
    created_at: u64,
}

impl Default for HeartbeatRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HeartbeatRegistry {
    /// Create a new registry with all heartbeats at zero (never seen).
    #[must_use]
    pub fn new() -> Self {
        Self {
            discovery: AtomicU64::new(0),
            capture: AtomicU64::new(0),
            persistence: AtomicU64::new(0),
            maintenance: AtomicU64::new(0),
            created_at: epoch_ms(),
        }
    }

    /// Record a heartbeat for the discovery subsystem.
    pub fn record_discovery(&self) {
        self.discovery.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Record a heartbeat for the capture subsystem.
    pub fn record_capture(&self) {
        self.capture.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Record a heartbeat for the persistence subsystem.
    pub fn record_persistence(&self) {
        self.persistence.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Record a heartbeat for the maintenance subsystem.
    pub fn record_maintenance(&self) {
        self.maintenance.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Read the last heartbeat timestamp for a component (epoch ms, 0 = never).
    fn last_heartbeat(&self, component: Component) -> u64 {
        match component {
            Component::Discovery => self.discovery.load(Ordering::SeqCst),
            Component::Capture => self.capture.load(Ordering::SeqCst),
            Component::Persistence => self.persistence.load(Ordering::SeqCst),
            Component::Maintenance => self.maintenance.load(Ordering::SeqCst),
        }
    }

    /// Check all components against their thresholds and return overall health.
    #[must_use]
    pub fn check_health(&self, config: &WatchdogConfig) -> HealthReport {
        let now = epoch_ms();
        let uptime_ms = now.saturating_sub(self.created_at);
        let components = [
            (Component::Discovery, config.discovery_stale_ms),
            (Component::Capture, config.capture_stale_ms),
            (Component::Persistence, config.persistence_stale_ms),
            (Component::Maintenance, config.maintenance_stale_ms),
        ];

        let mut statuses = Vec::with_capacity(components.len());
        let mut worst = HealthStatus::Healthy;

        for (component, threshold_ms) in components {
            let last = self.last_heartbeat(component);
            let status = if last == 0 {
                // Never recorded — may not have started yet.  Treat as
                // healthy within the grace period, degraded after.
                if uptime_ms < config.grace_period_ms {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Degraded
                }
            } else {
                let age_ms = now.saturating_sub(last);
                if age_ms <= threshold_ms {
                    HealthStatus::Healthy
                } else if age_ms <= threshold_ms.saturating_mul(2) {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::Critical
                }
            };

            if status > worst {
                worst = status;
            }

            statuses.push(ComponentHealth {
                component,
                last_heartbeat_ms: if last == 0 { None } else { Some(last) },
                age_ms: if last == 0 {
                    None
                } else {
                    Some(now.saturating_sub(last))
                },
                threshold_ms,
                status,
            });
        }

        HealthReport {
            timestamp_ms: now,
            overall: worst,
            components: statuses,
        }
    }
}

/// Watchdog configuration: per-component staleness thresholds.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// How often the monitor task runs (ms).
    pub check_interval: Duration,
    /// Discovery heartbeat stale after this many ms.
    pub discovery_stale_ms: u64,
    /// Capture heartbeat stale after this many ms.
    pub capture_stale_ms: u64,
    /// Persistence heartbeat stale after this many ms.
    pub persistence_stale_ms: u64,
    /// Maintenance heartbeat stale after this many ms.
    pub maintenance_stale_ms: u64,
    /// Grace period after startup (ms) before flagging missing heartbeats.
    pub grace_period_ms: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            discovery_stale_ms: 15_000, // 15 s  (discovery runs every 5 s)
            capture_stale_ms: 5_000,    //  5 s  (capture ticks every ~10 ms)
            persistence_stale_ms: 30_000, // 30 s  (depends on capture throughput)
            maintenance_stale_ms: 120_000, //  2 m  (maintenance runs every 60 s)
            grace_period_ms: 30_000,    // 30 s  after startup
        }
    }
}

/// Monitored subsystem identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Component {
    Discovery,
    Capture,
    Persistence,
    Maintenance,
}

impl std::fmt::Display for Component {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Discovery => write!(f, "discovery"),
            Self::Capture => write!(f, "capture"),
            Self::Persistence => write!(f, "persistence"),
            Self::Maintenance => write!(f, "maintenance"),
        }
    }
}

/// Health status ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Critical,
    /// Component is almost certainly hung (z-score >= 5 in adaptive mode).
    Hung,
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Critical => write!(f, "critical"),
            Self::Hung => write!(f, "hung"),
        }
    }
}

/// Per-component health details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentHealth {
    pub component: Component,
    /// Last heartbeat timestamp (epoch ms), `None` if never recorded.
    pub last_heartbeat_ms: Option<u64>,
    /// Age since last heartbeat (ms), `None` if never recorded.
    pub age_ms: Option<u64>,
    /// Configured threshold for this component (ms).
    pub threshold_ms: u64,
    pub status: HealthStatus,
}

/// Full health report across all components.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub timestamp_ms: u64,
    pub overall: HealthStatus,
    pub components: Vec<ComponentHealth>,
}

impl HealthReport {
    /// Return components that are not healthy.
    #[must_use]
    pub fn unhealthy_components(&self) -> Vec<&ComponentHealth> {
        self.components
            .iter()
            .filter(|c| c.status != HealthStatus::Healthy)
            .collect()
    }
}

/// Handle returned by [`spawn_watchdog`] to control the monitor task.
pub struct WatchdogHandle {
    task: JoinHandle<()>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl WatchdogHandle {
    /// Signal the watchdog to stop.
    pub fn signal_shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Wait for the watchdog task to finish.
    pub async fn join(self) {
        let _ = self.task.await;
    }
}

/// Spawn the watchdog monitor task.
///
/// The monitor periodically calls [`HeartbeatRegistry::check_health`] and
/// logs structured warnings for any unhealthy components.  It does **not**
/// perform forced restarts; that will be added in a future iteration.
///
/// # Arguments
/// * `heartbeats` – shared heartbeat registry updated by runtime tasks.
/// * `config` – staleness thresholds and check interval.
/// * `shutdown_flag` – external shutdown signal (e.g. from `ObservationRuntime`).
#[must_use]
pub fn spawn_watchdog(
    heartbeats: Arc<HeartbeatRegistry>,
    config: WatchdogConfig,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) -> WatchdogHandle {
    let internal_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let internal_flag = Arc::clone(&internal_shutdown);
    let check_interval = config.check_interval;

    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(check_interval);

        loop {
            interval.tick().await;

            if shutdown_flag.load(Ordering::SeqCst) || internal_flag.load(Ordering::SeqCst) {
                info!("Watchdog: shutdown signal received");
                break;
            }

            let report = heartbeats.check_health(&config);

            match report.overall {
                HealthStatus::Healthy => {
                    // Everything fine — nothing to log at info level.
                }
                HealthStatus::Degraded => {
                    for ch in report.unhealthy_components() {
                        warn!(
                            component = %ch.component,
                            status = %ch.status,
                            age_ms = ch.age_ms,
                            threshold_ms = ch.threshold_ms,
                            "Watchdog: component heartbeat is stale"
                        );
                    }
                }
                HealthStatus::Critical | HealthStatus::Hung => {
                    for ch in report.unhealthy_components() {
                        error!(
                            component = %ch.component,
                            status = %ch.status,
                            age_ms = ch.age_ms,
                            threshold_ms = ch.threshold_ms,
                            "Watchdog: component heartbeat critically stale"
                        );
                    }

                    // Dump full diagnostic report at error level.
                    if let Ok(json) = serde_json::to_string_pretty(&report) {
                        error!(diagnostic = %json, "Watchdog: diagnostic dump");
                    }
                }
            }
        }
    });

    WatchdogHandle {
        task,
        shutdown: internal_shutdown,
    }
}

// =============================================================================
// Mux Server Watchdog
// =============================================================================

/// Configuration for the mux server watchdog.
#[derive(Debug, Clone)]
pub struct MuxWatchdogConfig {
    /// How often to check mux server health (default: 30s).
    pub check_interval: Duration,
    /// Timeout for the ping health check (default: 5s).
    pub ping_timeout: Duration,
    /// Consecutive failures before reporting to DegradationManager.
    pub failure_threshold: u32,
    /// RSS memory warning threshold in bytes (default: 32 GB).
    pub memory_warning_bytes: u64,
    /// RSS memory critical threshold in bytes (default: 64 GB).
    pub memory_critical_bytes: u64,
    /// Ring buffer capacity for health history.
    pub history_capacity: usize,
}

impl Default for MuxWatchdogConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(30),
            ping_timeout: Duration::from_secs(5),
            failure_threshold: 3,
            memory_warning_bytes: 32 * 1024 * 1024 * 1024, // 32 GB
            memory_critical_bytes: 64 * 1024 * 1024 * 1024, // 64 GB
            history_capacity: 1000,
        }
    }
}

/// Result of a single mux server health check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxHealthSample {
    /// Epoch ms when sample was taken.
    pub timestamp_ms: u64,
    /// Whether the ping succeeded.
    pub ping_ok: bool,
    /// Ping latency in milliseconds (None if failed).
    pub ping_latency_ms: Option<u64>,
    /// Resident set size in bytes (None if unavailable).
    pub rss_bytes: Option<u64>,
    /// Health status derived from this sample.
    pub status: HealthStatus,
}

/// Mux server health report with history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxHealthReport {
    pub timestamp_ms: u64,
    pub status: HealthStatus,
    pub consecutive_failures: u32,
    pub latest_sample: Option<MuxHealthSample>,
    pub total_checks: u64,
    pub total_failures: u64,
}

/// Mux server watchdog — monitors mux server health and reports to DegradationManager.
pub struct MuxWatchdog {
    config: MuxWatchdogConfig,
    wezterm: crate::wezterm::WeztermHandle,
    /// Ring buffer of recent health samples.
    history: std::collections::VecDeque<MuxHealthSample>,
    consecutive_failures: u32,
    total_checks: u64,
    total_failures: u64,
}

impl MuxWatchdog {
    /// Create a new mux watchdog.
    #[must_use]
    pub fn new(config: MuxWatchdogConfig, wezterm: crate::wezterm::WeztermHandle) -> Self {
        Self {
            config,
            wezterm,
            history: std::collections::VecDeque::with_capacity(1000),
            consecutive_failures: 0,
            total_checks: 0,
            total_failures: 0,
        }
    }

    /// Run a single health check and return the sample.
    pub async fn check(&mut self) -> MuxHealthSample {
        self.total_checks += 1;
        let now = epoch_ms();
        let start = std::time::Instant::now();

        // Ping: try listing panes with timeout
        let ping_ok = matches!(
            tokio::time::timeout(self.config.ping_timeout, self.wezterm.list_panes()).await,
            Ok(Ok(_))
        );

        let ping_latency_ms = if ping_ok {
            Some(start.elapsed().as_millis() as u64)
        } else {
            None
        };

        // Memory check: get mux server RSS
        let rss_bytes = get_mux_server_rss().await;

        // Determine status
        let status = if !ping_ok {
            self.consecutive_failures += 1;
            self.total_failures += 1;
            if self.consecutive_failures >= self.config.failure_threshold {
                HealthStatus::Critical
            } else {
                HealthStatus::Degraded
            }
        } else {
            self.consecutive_failures = 0;
            match rss_bytes {
                Some(rss) if rss >= self.config.memory_critical_bytes => HealthStatus::Critical,
                Some(rss) if rss >= self.config.memory_warning_bytes => HealthStatus::Degraded,
                _ => HealthStatus::Healthy,
            }
        };

        let sample = MuxHealthSample {
            timestamp_ms: now,
            ping_ok,
            ping_latency_ms,
            rss_bytes,
            status,
        };

        // Store in history ring buffer
        if self.history.len() >= self.config.history_capacity {
            self.history.pop_front();
        }
        self.history.push_back(sample.clone());

        sample
    }

    /// Get the current health report.
    #[must_use]
    pub fn report(&self) -> MuxHealthReport {
        MuxHealthReport {
            timestamp_ms: epoch_ms(),
            status: self
                .history
                .back()
                .map_or(HealthStatus::Healthy, |s| s.status),
            consecutive_failures: self.consecutive_failures,
            latest_sample: self.history.back().cloned(),
            total_checks: self.total_checks,
            total_failures: self.total_failures,
        }
    }
}

/// Spawn the mux watchdog as a background task.
#[must_use]
pub fn spawn_mux_watchdog(
    config: MuxWatchdogConfig,
    wezterm: crate::wezterm::WeztermHandle,
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
) -> JoinHandle<()> {
    let check_interval = config.check_interval;
    let failure_threshold = config.failure_threshold;

    tokio::spawn(async move {
        let mut watchdog = MuxWatchdog::new(config, wezterm);
        let mut interval = tokio::time::interval(check_interval);

        info!("Mux watchdog started");

        loop {
            interval.tick().await;

            if shutdown_flag.load(Ordering::SeqCst) {
                info!("Mux watchdog shutting down");
                break;
            }

            let sample = watchdog.check().await;

            match sample.status {
                HealthStatus::Healthy => {
                    if watchdog.total_checks % 10 == 0 {
                        info!(
                            ping_ms = sample.ping_latency_ms,
                            rss_mb = sample.rss_bytes.map(|b| b / (1024 * 1024)),
                            "Mux watchdog: healthy"
                        );
                    }
                }
                HealthStatus::Degraded => {
                    warn!(
                        consecutive_failures = watchdog.consecutive_failures,
                        rss_mb = sample.rss_bytes.map(|b| b / (1024 * 1024)),
                        ping_ok = sample.ping_ok,
                        "Mux watchdog: degraded"
                    );
                    crate::degradation::enter_degraded(
                        crate::degradation::Subsystem::WeztermCli,
                        format!(
                            "Mux health degraded: {} consecutive failures",
                            watchdog.consecutive_failures
                        ),
                    );
                }
                HealthStatus::Critical | HealthStatus::Hung => {
                    error!(
                        consecutive_failures = watchdog.consecutive_failures,
                        rss_mb = sample.rss_bytes.map(|b| b / (1024 * 1024)),
                        ping_ok = sample.ping_ok,
                        threshold = failure_threshold,
                        "Mux watchdog: CRITICAL — mux server unresponsive or memory critical"
                    );
                    crate::degradation::enter_degraded(
                        crate::degradation::Subsystem::WeztermCli,
                        format!(
                            "Mux health critical: {} consecutive failures, RSS={} MB",
                            watchdog.consecutive_failures,
                            sample.rss_bytes.map_or(0, |b| b / (1024 * 1024))
                        ),
                    );
                }
            }
        }
    })
}

/// Get the RSS (resident set size) of the wezterm-mux-server process.
async fn get_mux_server_rss() -> Option<u64> {
    tokio::task::spawn_blocking(get_mux_server_rss_sync)
        .await
        .ok()
        .flatten()
}

/// Synchronous RSS lookup for wezterm-mux-server.
#[cfg(target_os = "macos")]
fn get_mux_server_rss_sync() -> Option<u64> {
    use std::process::Command;

    // Find the mux server PID
    let pgrep = Command::new("pgrep")
        .args(["-f", "wezterm-mux-server"])
        .output()
        .ok()?;

    if !pgrep.status.success() {
        return None;
    }

    let pid_str = String::from_utf8_lossy(&pgrep.stdout);
    let pid: u32 = pid_str.lines().next()?.trim().parse().ok()?;

    // Get RSS via ps (in KB on macOS)
    let ps = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;

    if !ps.status.success() {
        return None;
    }

    let rss_kb: u64 = String::from_utf8_lossy(&ps.stdout).trim().parse().ok()?;

    Some(rss_kb * 1024) // Convert KB to bytes
}

/// Synchronous RSS lookup for wezterm-mux-server.
#[cfg(target_os = "linux")]
fn get_mux_server_rss_sync() -> Option<u64> {
    use std::process::Command;

    // Find the mux server PID
    let pgrep = Command::new("pgrep")
        .args(["-f", "wezterm-mux-server"])
        .output()
        .ok()?;

    if !pgrep.status.success() {
        return None;
    }

    let pid_str = String::from_utf8_lossy(&pgrep.stdout);
    let pid: &str = pid_str.lines().next()?.trim();

    // Read VmRSS from /proc/<pid>/status
    let status_path = format!("/proc/{pid}/status");
    let contents = std::fs::read_to_string(status_path).ok()?;

    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let rest = rest.trim();
            // Format: "12345 kB"
            let kb_str = rest.split_whitespace().next()?;
            let rss_kb: u64 = kb_str.parse().ok()?;
            return Some(rss_kb * 1024); // Convert KB to bytes
        }
    }

    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn get_mux_server_rss_sync() -> Option<u64> {
    None
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn heartbeat_registry_defaults_to_zero() {
        let reg = HeartbeatRegistry::new();
        assert_eq!(reg.last_heartbeat(Component::Discovery), 0);
        assert_eq!(reg.last_heartbeat(Component::Capture), 0);
        assert_eq!(reg.last_heartbeat(Component::Persistence), 0);
        assert_eq!(reg.last_heartbeat(Component::Maintenance), 0);
    }

    #[test]
    fn record_updates_heartbeat() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        let ts = reg.last_heartbeat(Component::Discovery);
        assert!(ts > 0, "heartbeat should be set after record");
    }

    #[test]
    fn fresh_registry_is_healthy_within_grace_period() {
        let reg = HeartbeatRegistry::new();
        let config = WatchdogConfig {
            grace_period_ms: u64::MAX, // huge grace period
            ..WatchdogConfig::default()
        };
        let report = reg.check_health(&config);
        assert_eq!(report.overall, HealthStatus::Healthy);
    }

    #[test]
    fn active_heartbeats_are_healthy() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();

        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert_eq!(report.overall, HealthStatus::Healthy);
        assert!(report.unhealthy_components().is_empty());
    }

    #[test]
    fn stale_heartbeat_is_degraded() {
        let reg = HeartbeatRegistry::new();
        // Simulate a heartbeat that was recorded in the past.
        let past = epoch_ms().saturating_sub(20_000); // 20 s ago
        reg.discovery.store(past, Ordering::SeqCst);
        // Discovery threshold is 15 s, so 20 s is degraded (< 30 s critical).
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();

        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert_eq!(report.overall, HealthStatus::Degraded);

        let unhealthy = report.unhealthy_components();
        assert_eq!(unhealthy.len(), 1);
        assert_eq!(unhealthy[0].component, Component::Discovery);
    }

    #[test]
    fn very_stale_heartbeat_is_critical() {
        let reg = HeartbeatRegistry::new();
        // 60 s ago — well past 2×15 s critical threshold.
        let past = epoch_ms().saturating_sub(60_000);
        reg.discovery.store(past, Ordering::SeqCst);
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();

        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert_eq!(report.overall, HealthStatus::Critical);
    }

    #[test]
    fn health_report_serializes() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();

        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        let json = serde_json::to_string(&report).unwrap();
        let parsed: HealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.overall, report.overall);
        assert_eq!(parsed.components.len(), 4);
    }

    #[tokio::test]
    async fn watchdog_shuts_down_on_signal() {
        let heartbeats = Arc::new(HeartbeatRegistry::new());
        heartbeats.record_discovery();
        heartbeats.record_capture();
        heartbeats.record_persistence();
        heartbeats.record_maintenance();

        let shutdown = Arc::new(AtomicBool::new(false));
        let config = WatchdogConfig {
            check_interval: Duration::from_millis(10),
            ..WatchdogConfig::default()
        };

        let handle = spawn_watchdog(Arc::clone(&heartbeats), config, Arc::clone(&shutdown));

        // Let it run a few ticks.
        tokio::time::sleep(Duration::from_millis(50)).await;

        shutdown.store(true, Ordering::SeqCst);
        handle.join().await;
        // If we get here, shutdown worked.
    }

    #[test]
    fn component_display() {
        assert_eq!(Component::Discovery.to_string(), "discovery");
        assert_eq!(Component::Capture.to_string(), "capture");
        assert_eq!(Component::Persistence.to_string(), "persistence");
        assert_eq!(Component::Maintenance.to_string(), "maintenance");
    }

    #[test]
    fn health_status_ordering() {
        assert!(HealthStatus::Healthy < HealthStatus::Degraded);
        assert!(HealthStatus::Degraded < HealthStatus::Critical);
        assert!(HealthStatus::Critical < HealthStatus::Hung);
    }

    // =================================================================
    // MuxWatchdog tests
    // =================================================================

    #[test]
    fn mux_watchdog_config_defaults() {
        let config = MuxWatchdogConfig::default();
        assert_eq!(config.check_interval, Duration::from_secs(30));
        assert_eq!(config.ping_timeout, Duration::from_secs(5));
        assert_eq!(config.failure_threshold, 3);
        assert_eq!(config.memory_warning_bytes, 32 * 1024 * 1024 * 1024);
        assert_eq!(config.memory_critical_bytes, 64 * 1024 * 1024 * 1024);
        assert_eq!(config.history_capacity, 1000);
    }

    #[test]
    fn mux_health_sample_serializes() {
        let sample = MuxHealthSample {
            timestamp_ms: 1_700_000_000_000,
            ping_ok: true,
            ping_latency_ms: Some(5),
            rss_bytes: Some(1024 * 1024 * 100),
            status: HealthStatus::Healthy,
        };
        let json = serde_json::to_string(&sample).unwrap();
        assert!(json.contains("\"ping_ok\":true"));
        assert!(json.contains("\"ping_latency_ms\":5"));
        let parsed: MuxHealthSample = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, HealthStatus::Healthy);
    }

    #[test]
    fn mux_health_report_serializes() {
        let report = MuxHealthReport {
            timestamp_ms: 1_700_000_000_000,
            status: HealthStatus::Healthy,
            consecutive_failures: 0,
            latest_sample: None,
            total_checks: 10,
            total_failures: 1,
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"total_checks\":10"));
        let parsed: MuxHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_failures, 1);
    }

    #[test]
    fn mux_watchdog_initial_report_is_healthy() {
        let config = MuxWatchdogConfig::default();
        let wezterm = crate::wezterm::mock_wezterm_handle();
        let watchdog = MuxWatchdog::new(config, wezterm);
        let report = watchdog.report();
        assert_eq!(report.status, HealthStatus::Healthy);
        assert_eq!(report.consecutive_failures, 0);
        assert_eq!(report.total_checks, 0);
    }

    #[tokio::test]
    async fn mux_watchdog_records_successful_check() {
        let config = MuxWatchdogConfig::default();
        let wezterm = crate::wezterm::mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        let sample = watchdog.check().await;
        assert!(sample.ping_ok);
        assert_eq!(sample.status, HealthStatus::Healthy);
        assert_eq!(watchdog.consecutive_failures, 0);
        assert_eq!(watchdog.total_checks, 1);
        assert_eq!(watchdog.history.len(), 1);
    }

    #[tokio::test]
    async fn mux_watchdog_detects_failure() {
        let config = MuxWatchdogConfig {
            failure_threshold: 2,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = crate::wezterm::mock_wezterm_handle_failing();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        // First failure: degraded
        let sample = watchdog.check().await;
        assert!(!sample.ping_ok);
        assert_eq!(sample.status, HealthStatus::Degraded);
        assert_eq!(watchdog.consecutive_failures, 1);

        // Second failure: critical (meets threshold)
        let sample = watchdog.check().await;
        assert_eq!(sample.status, HealthStatus::Critical);
        assert_eq!(watchdog.consecutive_failures, 2);
    }

    #[tokio::test]
    async fn mux_watchdog_resets_on_success() {
        let config = MuxWatchdogConfig::default();
        let wezterm = crate::wezterm::mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        // Simulate prior failures
        watchdog.consecutive_failures = 5;
        watchdog.total_failures = 5;

        let sample = watchdog.check().await;
        assert!(sample.ping_ok);
        assert_eq!(watchdog.consecutive_failures, 0);
        // total_failures should NOT reset
        assert_eq!(watchdog.total_failures, 5);
    }

    #[tokio::test]
    async fn mux_watchdog_history_bounded() {
        let config = MuxWatchdogConfig {
            history_capacity: 3,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = crate::wezterm::mock_wezterm_handle();
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        for _ in 0..5 {
            watchdog.check().await;
        }

        assert_eq!(watchdog.history.len(), 3, "history should be bounded");
        assert_eq!(watchdog.total_checks, 5);
    }
}
