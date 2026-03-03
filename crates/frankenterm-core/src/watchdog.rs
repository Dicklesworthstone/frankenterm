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

use crate::runtime_compat::task::{self, JoinHandle};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

// =============================================================================
// Telemetry types
// =============================================================================

/// Operational telemetry for [`HeartbeatRegistry`].
#[derive(Debug, Default)]
pub struct WatchdogTelemetry {
    discovery_heartbeats: AtomicU64,
    capture_heartbeats: AtomicU64,
    persistence_heartbeats: AtomicU64,
    maintenance_heartbeats: AtomicU64,
    health_checks: AtomicU64,
}

impl WatchdogTelemetry {
    pub fn snapshot(&self) -> WatchdogTelemetrySnapshot {
        WatchdogTelemetrySnapshot {
            discovery_heartbeats: self.discovery_heartbeats.load(Ordering::Relaxed),
            capture_heartbeats: self.capture_heartbeats.load(Ordering::Relaxed),
            persistence_heartbeats: self.persistence_heartbeats.load(Ordering::Relaxed),
            maintenance_heartbeats: self.maintenance_heartbeats.load(Ordering::Relaxed),
            health_checks: self.health_checks.load(Ordering::Relaxed),
        }
    }
}

/// Serializable telemetry snapshot for [`HeartbeatRegistry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchdogTelemetrySnapshot {
    pub discovery_heartbeats: u64,
    pub capture_heartbeats: u64,
    pub persistence_heartbeats: u64,
    pub maintenance_heartbeats: u64,
    pub health_checks: u64,
}

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
    telemetry: WatchdogTelemetry,
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
            telemetry: WatchdogTelemetry::default(),
        }
    }

    /// Record a heartbeat for the discovery subsystem.
    pub fn record_discovery(&self) {
        self.telemetry
            .discovery_heartbeats
            .fetch_add(1, Ordering::Relaxed);
        self.discovery.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Record a heartbeat for the capture subsystem.
    pub fn record_capture(&self) {
        self.telemetry
            .capture_heartbeats
            .fetch_add(1, Ordering::Relaxed);
        self.capture.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Record a heartbeat for the persistence subsystem.
    pub fn record_persistence(&self) {
        self.telemetry
            .persistence_heartbeats
            .fetch_add(1, Ordering::Relaxed);
        self.persistence.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Record a heartbeat for the maintenance subsystem.
    pub fn record_maintenance(&self) {
        self.telemetry
            .maintenance_heartbeats
            .fetch_add(1, Ordering::Relaxed);
        self.maintenance.store(epoch_ms(), Ordering::SeqCst);
    }

    /// Epoch ms when the registry was created.
    #[must_use]
    pub fn created_at_ms(&self) -> u64 {
        self.created_at
    }

    /// Read the last heartbeat timestamp for a component (epoch ms, 0 = never).
    pub fn last_heartbeat(&self, component: Component) -> u64 {
        match component {
            Component::Discovery => self.discovery.load(Ordering::SeqCst),
            Component::Capture => self.capture.load(Ordering::SeqCst),
            Component::Persistence => self.persistence.load(Ordering::SeqCst),
            Component::Maintenance => self.maintenance.load(Ordering::SeqCst),
        }
    }

    /// Returns the telemetry tracker for this registry.
    pub fn telemetry(&self) -> &WatchdogTelemetry {
        &self.telemetry
    }

    /// Check all components against their thresholds and return overall health.
    #[must_use]
    pub fn check_health(&self, config: &WatchdogConfig) -> HealthReport {
        self.telemetry.health_checks.fetch_add(1, Ordering::Relaxed);
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

impl Component {
    /// All component variants.
    pub const ALL: [Component; 4] = [
        Component::Discovery,
        Component::Capture,
        Component::Persistence,
        Component::Maintenance,
    ];
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

    let task = task::spawn(async move {
        loop {
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

            // Use the dual-runtime sleep helper during the tokio -> asupersync migration.
            crate::runtime_compat::sleep(check_interval).await;
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
    /// Backend-specific warning lines surfaced by `WeztermInterface::watchdog_warnings`.
    #[serde(default)]
    pub watchdog_warnings: Vec<String>,
    /// Number of warning lines captured during this check.
    #[serde(default)]
    pub warning_count: usize,
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

#[derive(Debug, Clone)]
enum WarningProbeOutcome {
    Ok(Vec<String>),
    Error(String),
}

fn warning_status_from_line(line: &str) -> Option<HealthStatus> {
    let normalized = line.to_ascii_lowercase();

    if normalized.contains("hung")
        || normalized.contains("unresponsive")
        || normalized.contains("deadlock")
    {
        return Some(HealthStatus::Hung);
    }

    if normalized.contains("critical")
        || normalized.contains("fatal")
        || normalized.contains("circuit open")
        || normalized.contains("panic")
    {
        return Some(HealthStatus::Critical);
    }

    if normalized.contains("degraded")
        || normalized.contains("warning")
        || normalized.contains("half-open")
        || normalized.contains("failed")
        || normalized.contains("timeout")
    {
        return Some(HealthStatus::Degraded);
    }

    None
}

fn warning_status_from_lines(lines: &[String]) -> Option<HealthStatus> {
    if lines.is_empty() {
        return None;
    }

    let mut derived = HealthStatus::Degraded;
    for line in lines {
        derived = derived.max(warning_status_from_line(line).unwrap_or(HealthStatus::Degraded));
    }
    Some(derived)
}

impl MuxWatchdog {
    /// Create a new mux watchdog.
    #[must_use]
    pub fn new(mut config: MuxWatchdogConfig, wezterm: crate::wezterm::WeztermHandle) -> Self {
        // Keep at least one slot so history bookkeeping stays well-defined.
        let history_capacity = config.history_capacity.max(1);
        config.history_capacity = history_capacity;
        Self {
            config,
            wezterm,
            history: std::collections::VecDeque::with_capacity(history_capacity),
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
            crate::runtime_compat::timeout(self.config.ping_timeout, self.wezterm.list_panes())
                .await,
            Ok(Ok(_))
        );

        let ping_latency_ms = if ping_ok {
            Some(start.elapsed().as_millis() as u64)
        } else {
            None
        };

        // Memory check: get mux server RSS
        let rss_bytes = get_mux_server_rss().await;

        // Query backend-specific warning lines (e.g., shard health warnings).
        let warning_probe = match crate::runtime_compat::timeout(
            self.config.ping_timeout,
            self.wezterm.watchdog_warnings(),
        )
        .await
        {
            Ok(Ok(lines)) => WarningProbeOutcome::Ok(lines),
            Ok(Err(err)) => WarningProbeOutcome::Error(format!(
                "failed to query backend watchdog warnings: {err}"
            )),
            Err(_) => WarningProbeOutcome::Error(format!(
                "timed out querying backend watchdog warnings after {} ms",
                self.config.ping_timeout.as_millis()
            )),
        };

        let mut watchdog_warnings = match warning_probe {
            WarningProbeOutcome::Ok(lines) => lines,
            WarningProbeOutcome::Error(err) => vec![err],
        };
        watchdog_warnings.retain(|line| !line.trim().is_empty());

        // Determine baseline status from ping/memory checks.
        let mut status = if !ping_ok {
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

        // Fold warning severity into the final status.
        if let Some(warning_status) = warning_status_from_lines(&watchdog_warnings) {
            status = status.max(warning_status);
        }

        let warning_count = watchdog_warnings.len();

        let sample = MuxHealthSample {
            timestamp_ms: now,
            ping_ok,
            ping_latency_ms,
            rss_bytes,
            watchdog_warnings,
            warning_count,
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

    task::spawn(async move {
        let mut watchdog = MuxWatchdog::new(config, wezterm);

        info!("Mux watchdog started");

        loop {
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
                            warning_count = sample.warning_count,
                            warning_details = sample.watchdog_warnings.join(" | "),
                            "Mux watchdog: healthy"
                        );
                    }
                }
                HealthStatus::Degraded => {
                    warn!(
                        consecutive_failures = watchdog.consecutive_failures,
                        rss_mb = sample.rss_bytes.map(|b| b / (1024 * 1024)),
                        ping_ok = sample.ping_ok,
                        warning_count = sample.warning_count,
                        warning_details = sample.watchdog_warnings.join(" | "),
                        "Mux watchdog: degraded"
                    );
                    crate::degradation::enter_degraded(
                        crate::degradation::Subsystem::WeztermCli,
                        format!(
                            "Mux health degraded: {} consecutive failures, warnings={}",
                            watchdog.consecutive_failures, sample.warning_count
                        ),
                    );
                }
                HealthStatus::Critical | HealthStatus::Hung => {
                    error!(
                        consecutive_failures = watchdog.consecutive_failures,
                        rss_mb = sample.rss_bytes.map(|b| b / (1024 * 1024)),
                        ping_ok = sample.ping_ok,
                        threshold = failure_threshold,
                        warning_count = sample.warning_count,
                        warning_details = sample.watchdog_warnings.join(" | "),
                        "Mux watchdog: CRITICAL — mux server unresponsive or memory critical"
                    );
                    crate::degradation::enter_degraded(
                        crate::degradation::Subsystem::WeztermCli,
                        format!(
                            "Mux health critical: {} consecutive failures, RSS={} MB, warnings={}",
                            watchdog.consecutive_failures,
                            sample.rss_bytes.map_or(0, |b| b / (1024 * 1024)),
                            sample.warning_count
                        ),
                    );
                }
            }

            // Use the dual-runtime sleep helper during the tokio -> asupersync migration.
            crate::runtime_compat::sleep(check_interval).await;
        }
    })
}

/// Get the RSS (resident set size) of the wezterm-mux-server process.
async fn get_mux_server_rss() -> Option<u64> {
    crate::runtime_compat::spawn_blocking(get_mux_server_rss_sync)
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

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build watchdog test runtime");
        runtime.block_on(future);
    }

    #[test]
    fn watchdog_shuts_down_on_signal() {
        run_async_test(async {
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
            crate::runtime_compat::sleep(Duration::from_millis(50)).await;

            shutdown.store(true, Ordering::SeqCst);
            handle.join().await;
            // If we get here, shutdown worked.
        });
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
    fn mux_watchdog_new_uses_configured_history_capacity() {
        let config = MuxWatchdogConfig {
            history_capacity: 7,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = crate::wezterm::mock_wezterm_handle();
        let watchdog = MuxWatchdog::new(config, wezterm);
        assert_eq!(watchdog.config.history_capacity, 7);
        assert!(watchdog.history.capacity() >= 7);
    }

    #[test]
    fn mux_watchdog_new_clamps_zero_history_capacity() {
        let config = MuxWatchdogConfig {
            history_capacity: 0,
            ..MuxWatchdogConfig::default()
        };
        let wezterm = crate::wezterm::mock_wezterm_handle();
        let watchdog = MuxWatchdog::new(config, wezterm);
        assert_eq!(watchdog.config.history_capacity, 1);
        assert!(watchdog.history.capacity() >= 1);
    }

    #[test]
    fn mux_health_sample_serializes() {
        let sample = MuxHealthSample {
            timestamp_ms: 1_700_000_000_000,
            ping_ok: true,
            ping_latency_ms: Some(5),
            rss_bytes: Some(1024 * 1024 * 100),
            watchdog_warnings: vec![],
            warning_count: 0,
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

    #[test]
    fn mux_watchdog_records_successful_check() {
        run_async_test(async {
            let config = MuxWatchdogConfig::default();
            let wezterm = crate::wezterm::mock_wezterm_handle();
            let mut watchdog = MuxWatchdog::new(config, wezterm);

            let sample = watchdog.check().await;
            assert!(sample.ping_ok);
            assert_eq!(sample.status, HealthStatus::Healthy);
            assert_eq!(sample.warning_count, 0);
            assert!(sample.watchdog_warnings.is_empty());
            assert_eq!(watchdog.consecutive_failures, 0);
            assert_eq!(watchdog.total_checks, 1);
            assert_eq!(watchdog.history.len(), 1);
        });
    }

    #[test]
    fn mux_watchdog_detects_failure() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn mux_watchdog_resets_on_success() {
        run_async_test(async {
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
        });
    }

    #[test]
    fn mux_watchdog_history_bounded() {
        run_async_test(async {
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
        });
    }

    // ── HeartbeatRegistry additional tests ─────────────────────────

    #[test]
    fn heartbeat_registry_default_trait() {
        let reg = HeartbeatRegistry::default();
        assert_eq!(reg.last_heartbeat(Component::Discovery), 0);
        assert!(reg.created_at_ms() > 0);
    }

    #[test]
    fn created_at_ms_is_recent() {
        let before = epoch_ms();
        let reg = HeartbeatRegistry::new();
        let after = epoch_ms();
        assert!(reg.created_at_ms() >= before);
        assert!(reg.created_at_ms() <= after);
    }

    #[test]
    fn record_each_component_independently() {
        let reg = HeartbeatRegistry::new();
        reg.record_capture();
        assert!(reg.last_heartbeat(Component::Capture) > 0);
        assert_eq!(reg.last_heartbeat(Component::Discovery), 0);
        assert_eq!(reg.last_heartbeat(Component::Persistence), 0);
        assert_eq!(reg.last_heartbeat(Component::Maintenance), 0);
    }

    #[test]
    fn grace_period_expired_no_heartbeats_is_degraded() {
        let reg = HeartbeatRegistry::new();
        // Force created_at far in the past so grace period has expired
        let past_created = epoch_ms().saturating_sub(120_000);
        // We can't directly set created_at since it's not pub, but we can
        // use a config with a very short grace period
        let config = WatchdogConfig {
            grace_period_ms: 0, // No grace period
            ..WatchdogConfig::default()
        };
        let _ = past_created; // used conceptually
        let report = reg.check_health(&config);
        // With zero grace period, unrecorded heartbeats should be degraded
        assert!(report.overall >= HealthStatus::Degraded);
    }

    #[test]
    fn multiple_components_degraded() {
        let reg = HeartbeatRegistry::new();
        let past = epoch_ms().saturating_sub(20_000);
        reg.discovery.store(past, Ordering::SeqCst);
        reg.capture.store(past, Ordering::SeqCst);
        reg.record_persistence();
        reg.record_maintenance();

        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        let unhealthy = report.unhealthy_components();
        assert!(unhealthy.len() >= 2);
    }

    #[test]
    fn worst_status_propagates_to_overall() {
        let reg = HeartbeatRegistry::new();
        // One component critical, others healthy
        let far_past = epoch_ms().saturating_sub(60_000);
        reg.discovery.store(far_past, Ordering::SeqCst);
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();

        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert_eq!(report.overall, HealthStatus::Critical);
    }

    #[test]
    fn component_health_fields_populated() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        let config = WatchdogConfig {
            grace_period_ms: u64::MAX,
            ..WatchdogConfig::default()
        };
        let report = reg.check_health(&config);

        let discovery = report
            .components
            .iter()
            .find(|c| c.component == Component::Discovery)
            .unwrap();
        assert!(discovery.last_heartbeat_ms.is_some());
        assert!(discovery.age_ms.is_some());
        assert_eq!(discovery.threshold_ms, config.discovery_stale_ms);
        assert_eq!(discovery.status, HealthStatus::Healthy);

        // Unrecorded component within grace period
        let capture = report
            .components
            .iter()
            .find(|c| c.component == Component::Capture)
            .unwrap();
        assert!(capture.last_heartbeat_ms.is_none());
        assert!(capture.age_ms.is_none());
    }

    #[test]
    fn health_report_has_four_components() {
        let reg = HeartbeatRegistry::new();
        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert_eq!(report.components.len(), 4);
    }

    #[test]
    fn health_report_timestamp_is_recent() {
        let before = epoch_ms();
        let reg = HeartbeatRegistry::new();
        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert!(report.timestamp_ms >= before);
    }

    // ── WatchdogConfig ─────────────────────────────────────────────

    #[test]
    fn watchdog_config_default_values() {
        let config = WatchdogConfig::default();
        assert_eq!(config.check_interval, Duration::from_secs(30));
        assert_eq!(config.discovery_stale_ms, 15_000);
        assert_eq!(config.capture_stale_ms, 5_000);
        assert_eq!(config.persistence_stale_ms, 30_000);
        assert_eq!(config.maintenance_stale_ms, 120_000);
        assert_eq!(config.grace_period_ms, 30_000);
    }

    // ── Component ──────────────────────────────────────────────────

    #[test]
    fn component_all_has_four_variants() {
        assert_eq!(Component::ALL.len(), 4);
        let set: std::collections::HashSet<_> = Component::ALL.iter().collect();
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn component_serde_roundtrip() {
        for component in &Component::ALL {
            let json = serde_json::to_string(component).unwrap();
            let parsed: Component = serde_json::from_str(&json).unwrap();
            assert_eq!(*component, parsed);
        }
    }

    #[test]
    fn component_serde_uses_snake_case() {
        let json = serde_json::to_string(&Component::Discovery).unwrap();
        assert_eq!(json, r#""discovery""#);
    }

    // ── HealthStatus ───────────────────────────────────────────────

    #[test]
    fn health_status_display() {
        assert_eq!(HealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(HealthStatus::Degraded.to_string(), "degraded");
        assert_eq!(HealthStatus::Critical.to_string(), "critical");
        assert_eq!(HealthStatus::Hung.to_string(), "hung");
    }

    #[test]
    fn health_status_serde_roundtrip() {
        let statuses = [
            HealthStatus::Healthy,
            HealthStatus::Degraded,
            HealthStatus::Critical,
            HealthStatus::Hung,
        ];
        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let parsed: HealthStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*status, parsed);
        }
    }

    #[test]
    fn health_status_serde_uses_snake_case() {
        let json = serde_json::to_string(&HealthStatus::Healthy).unwrap();
        assert_eq!(json, r#""healthy""#);
    }

    // ── HealthReport ───────────────────────────────────────────────

    #[test]
    fn unhealthy_components_returns_empty_when_all_healthy() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        reg.record_persistence();
        reg.record_maintenance();
        let config = WatchdogConfig::default();
        let report = reg.check_health(&config);
        assert!(report.unhealthy_components().is_empty());
    }

    // ── WatchdogHandle ─────────────────────────────────────────────

    #[test]
    fn watchdog_handle_signal_shutdown() {
        run_async_test(async {
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

            crate::runtime_compat::sleep(Duration::from_millis(30)).await;

            // Use handle's own signal_shutdown instead of the external flag
            handle.signal_shutdown();
            handle.join().await;
        });
    }

    // ── MuxWatchdog additional tests ───────────────────────────────

    #[test]
    fn mux_watchdog_report_reflects_latest_check() {
        run_async_test(async {
            let config = MuxWatchdogConfig::default();
            let wezterm = crate::wezterm::mock_wezterm_handle();
            let mut watchdog = MuxWatchdog::new(config, wezterm);

            assert!(watchdog.report().latest_sample.is_none());

            watchdog.check().await;
            let report = watchdog.report();
            assert!(report.latest_sample.is_some());
            assert_eq!(report.total_checks, 1);
        });
    }

    #[test]
    fn mux_watchdog_total_failures_accumulate() {
        run_async_test(async {
            let config = MuxWatchdogConfig {
                failure_threshold: 10,
                ..MuxWatchdogConfig::default()
            };
            let wezterm = crate::wezterm::mock_wezterm_handle_failing();
            let mut watchdog = MuxWatchdog::new(config, wezterm);

            for _ in 0..3 {
                watchdog.check().await;
            }

            assert_eq!(watchdog.total_failures, 3);
            assert_eq!(watchdog.total_checks, 3);
            assert_eq!(watchdog.consecutive_failures, 3);
        });
    }

    #[test]
    fn mux_health_report_serde_roundtrip_with_sample() {
        let sample = MuxHealthSample {
            timestamp_ms: 1_700_000_000_000,
            ping_ok: false,
            ping_latency_ms: None,
            rss_bytes: Some(1024 * 1024),
            watchdog_warnings: vec!["warning".to_string()],
            warning_count: 1,
            status: HealthStatus::Degraded,
        };
        let report = MuxHealthReport {
            timestamp_ms: 1_700_000_000_000,
            status: HealthStatus::Degraded,
            consecutive_failures: 2,
            latest_sample: Some(sample),
            total_checks: 5,
            total_failures: 2,
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: MuxHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.consecutive_failures, 2);
        assert!(parsed.latest_sample.is_some());
        assert_eq!(parsed.latest_sample.unwrap().status, HealthStatus::Degraded);
    }

    // -------------------------------------------------------------------
    // Batch: DarkBadger wa-1u90p.7.1
    // -------------------------------------------------------------------

    // -- WatchdogConfig --

    #[test]
    fn watchdog_config_debug_clone() {
        let config = WatchdogConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("WatchdogConfig"), "got: {}", dbg);
        let cloned = config.clone();
        assert_eq!(cloned.grace_period_ms, config.grace_period_ms);
    }

    // -- Component --

    #[test]
    fn component_debug_clone_copy() {
        let c = Component::Capture;
        let cloned = c;
        let copied = c;
        assert_eq!(cloned, copied);
    }

    #[test]
    fn component_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        for c in Component::ALL {
            set.insert(c);
        }
        assert_eq!(set.len(), 4);
    }

    #[test]
    fn component_eq_ne() {
        assert_eq!(Component::Discovery, Component::Discovery);
        assert_ne!(Component::Discovery, Component::Capture);
        assert_ne!(Component::Persistence, Component::Maintenance);
    }

    // -- HealthStatus --

    #[test]
    fn health_status_debug_clone_copy() {
        let s = HealthStatus::Degraded;
        let cloned = s;
        let copied = s;
        assert_eq!(cloned, copied);
    }

    #[test]
    fn health_status_display_all() {
        assert_eq!(HealthStatus::Healthy.to_string(), "healthy");
        assert_eq!(HealthStatus::Degraded.to_string(), "degraded");
        assert_eq!(HealthStatus::Critical.to_string(), "critical");
        assert_eq!(HealthStatus::Hung.to_string(), "hung");
    }

    #[test]
    fn health_status_ordering_complete() {
        assert!(HealthStatus::Healthy < HealthStatus::Degraded);
        assert!(HealthStatus::Degraded < HealthStatus::Critical);
        assert!(HealthStatus::Critical < HealthStatus::Hung);
    }

    #[test]
    fn health_status_eq_ne() {
        assert_eq!(HealthStatus::Healthy, HealthStatus::Healthy);
        assert_ne!(HealthStatus::Healthy, HealthStatus::Hung);
    }

    // -- ComponentHealth --

    #[test]
    fn component_health_debug_clone() {
        let ch = ComponentHealth {
            component: Component::Discovery,
            last_heartbeat_ms: Some(1000),
            age_ms: Some(500),
            threshold_ms: 30_000,
            status: HealthStatus::Healthy,
        };
        let dbg = format!("{:?}", ch);
        assert!(dbg.contains("ComponentHealth"), "got: {}", dbg);
        let cloned = ch.clone();
        assert_eq!(cloned.status, HealthStatus::Healthy);
    }

    #[test]
    fn component_health_serde_roundtrip() {
        let ch = ComponentHealth {
            component: Component::Capture,
            last_heartbeat_ms: None,
            age_ms: None,
            threshold_ms: 15_000,
            status: HealthStatus::Degraded,
        };
        let json = serde_json::to_string(&ch).unwrap();
        let back: ComponentHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(back.component, Component::Capture);
        assert_eq!(back.status, HealthStatus::Degraded);
    }

    // -- HealthReport --

    #[test]
    fn health_report_debug_clone() {
        let report = HealthReport {
            timestamp_ms: 1000,
            overall: HealthStatus::Healthy,
            components: vec![],
        };
        let dbg = format!("{:?}", report);
        assert!(dbg.contains("HealthReport"), "got: {}", dbg);
        let cloned = report.clone();
        assert_eq!(cloned.overall, HealthStatus::Healthy);
    }

    #[test]
    fn health_report_unhealthy_with_mixed() {
        let report = HealthReport {
            timestamp_ms: 1000,
            overall: HealthStatus::Critical,
            components: vec![
                ComponentHealth {
                    component: Component::Discovery,
                    last_heartbeat_ms: Some(900),
                    age_ms: Some(100),
                    threshold_ms: 30_000,
                    status: HealthStatus::Healthy,
                },
                ComponentHealth {
                    component: Component::Capture,
                    last_heartbeat_ms: None,
                    age_ms: None,
                    threshold_ms: 15_000,
                    status: HealthStatus::Critical,
                },
            ],
        };
        let unhealthy = report.unhealthy_components();
        assert_eq!(unhealthy.len(), 1);
        assert_eq!(unhealthy[0].component, Component::Capture);
    }

    // -- MuxWatchdogConfig --

    #[test]
    fn mux_watchdog_config_debug_clone() {
        let config = MuxWatchdogConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("MuxWatchdogConfig"), "got: {}", dbg);
        let cloned = config.clone();
        assert_eq!(cloned.failure_threshold, config.failure_threshold);
    }

    // -- MuxHealthSample --

    #[test]
    fn mux_health_sample_debug_clone() {
        let sample = MuxHealthSample {
            timestamp_ms: 1000,
            ping_ok: true,
            ping_latency_ms: Some(5),
            rss_bytes: Some(1024),
            watchdog_warnings: vec![],
            warning_count: 0,
            status: HealthStatus::Healthy,
        };
        let dbg = format!("{:?}", sample);
        assert!(dbg.contains("MuxHealthSample"), "got: {}", dbg);
        let cloned = sample.clone();
        assert_eq!(cloned.ping_ok, true);
    }

    #[test]
    fn warning_status_parser_maps_expected_tokens() {
        assert_eq!(
            warning_status_from_line("Shard 0 degraded due to half-open circuit"),
            Some(HealthStatus::Degraded)
        );
        assert_eq!(
            warning_status_from_line("Shard 1 critical circuit open state"),
            Some(HealthStatus::Critical)
        );
        assert_eq!(
            warning_status_from_line("Mux appears hung and unresponsive"),
            Some(HealthStatus::Hung)
        );
        assert_eq!(warning_status_from_line("plain note"), None);
    }

    #[test]
    fn mux_watchdog_escalates_on_critical_watchdog_warning() {
        run_async_test(async {
            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.set_watchdog_warnings(vec!["critical: shard 2 circuit open".to_string()])
                .await;
            let wezterm: crate::wezterm::WeztermHandle = mock;
            let mut watchdog = MuxWatchdog::new(MuxWatchdogConfig::default(), wezterm);

            let sample = watchdog.check().await;
            assert!(sample.ping_ok);
            assert_eq!(sample.status, HealthStatus::Critical);
            assert_eq!(sample.warning_count, 1);
            assert!(sample.watchdog_warnings[0].contains("critical"));
        });
    }

    #[test]
    fn mux_watchdog_warning_probe_failure_marks_degraded() {
        run_async_test(async {
            let mock = Arc::new(crate::wezterm::MockWezterm::new());
            mock.set_watchdog_warning_error(Some("probe transport unavailable".to_string()))
                .await;
            let wezterm: crate::wezterm::WeztermHandle = mock;
            let mut watchdog = MuxWatchdog::new(MuxWatchdogConfig::default(), wezterm);

            let sample = watchdog.check().await;
            assert!(sample.ping_ok);
            assert_eq!(sample.status, HealthStatus::Degraded);
            assert_eq!(sample.warning_count, 1);
            assert!(sample.watchdog_warnings[0].contains("failed to query"));
        });
    }

    // -- MuxHealthReport --

    #[test]
    fn mux_health_report_debug_clone() {
        let report = MuxHealthReport {
            timestamp_ms: 1000,
            status: HealthStatus::Healthy,
            consecutive_failures: 0,
            latest_sample: None,
            total_checks: 0,
            total_failures: 0,
        };
        let dbg = format!("{:?}", report);
        assert!(dbg.contains("MuxHealthReport"), "got: {}", dbg);
        let cloned = report.clone();
        assert_eq!(cloned.consecutive_failures, 0);
    }

    // ── Telemetry counter tests ──────────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let reg = HeartbeatRegistry::new();
        let snap = reg.telemetry().snapshot();
        assert_eq!(snap.discovery_heartbeats, 0);
        assert_eq!(snap.capture_heartbeats, 0);
        assert_eq!(snap.persistence_heartbeats, 0);
        assert_eq!(snap.maintenance_heartbeats, 0);
        assert_eq!(snap.health_checks, 0);
    }

    #[test]
    fn telemetry_discovery_counted() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_discovery();
        let snap = reg.telemetry().snapshot();
        assert_eq!(snap.discovery_heartbeats, 2);
    }

    #[test]
    fn telemetry_capture_counted() {
        let reg = HeartbeatRegistry::new();
        reg.record_capture();
        let snap = reg.telemetry().snapshot();
        assert_eq!(snap.capture_heartbeats, 1);
    }

    #[test]
    fn telemetry_persistence_counted() {
        let reg = HeartbeatRegistry::new();
        reg.record_persistence();
        reg.record_persistence();
        reg.record_persistence();
        let snap = reg.telemetry().snapshot();
        assert_eq!(snap.persistence_heartbeats, 3);
    }

    #[test]
    fn telemetry_maintenance_counted() {
        let reg = HeartbeatRegistry::new();
        reg.record_maintenance();
        let snap = reg.telemetry().snapshot();
        assert_eq!(snap.maintenance_heartbeats, 1);
    }

    #[test]
    fn telemetry_health_check_counted() {
        let reg = HeartbeatRegistry::new();
        let config = WatchdogConfig::default();
        let _ = reg.check_health(&config);
        let _ = reg.check_health(&config);
        let snap = reg.telemetry().snapshot();
        assert_eq!(snap.health_checks, 2);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let reg = HeartbeatRegistry::new();
        reg.record_discovery();
        reg.record_capture();
        let config = WatchdogConfig::default();
        let _ = reg.check_health(&config);
        let snap = reg.telemetry().snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: WatchdogTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }
}
