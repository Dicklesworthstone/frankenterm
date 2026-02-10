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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl std::fmt::Display for HealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Critical => write!(f, "critical"),
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
                HealthStatus::Critical => {
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
    }
}
