//! Connector host runtime lifecycle and protocol envelopes.
//!
//! This module provides a deterministic, testable host-runtime core for
//! connector-fabric embedding. It intentionally avoids side effects and uses
//! caller-provided timestamps so lifecycle behavior is reproducible in tests.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TRANSITION_HISTORY_CAPACITY: usize = 64;

/// Protocol version shared between the FrankenTerm control plane and connector host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectorProtocolVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
}

impl ConnectorProtocolVersion {
    /// Create a protocol version.
    #[must_use]
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl Default for ConnectorProtocolVersion {
    fn default() -> Self {
        Self::new(1, 0, 0)
    }
}

impl std::fmt::Display for ConnectorProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Normalized connector failure classes for deterministic automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorFailureClass {
    Auth,
    Quota,
    Network,
    Policy,
    Validation,
    Timeout,
    Unknown,
}

impl ConnectorFailureClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Quota => "quota",
            Self::Network => "network",
            Self::Policy => "policy",
            Self::Validation => "validation",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for ConnectorFailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Runtime budget guardrails to isolate connector execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorRuntimeBudgets {
    /// CPU budget in milliseconds available per second window.
    pub cpu_millis_per_second: u32,
    /// Memory budget in bytes.
    pub memory_bytes: u64,
    /// I/O throughput budget in bytes per second.
    pub io_bytes_per_second: u64,
    /// Maximum in-flight connector operations.
    pub max_inflight_ops: u32,
}

impl Default for ConnectorRuntimeBudgets {
    fn default() -> Self {
        Self {
            cpu_millis_per_second: 750,
            memory_bytes: 512 * 1024 * 1024,
            io_bytes_per_second: 16 * 1024 * 1024,
            max_inflight_ops: 256,
        }
    }
}

impl ConnectorRuntimeBudgets {
    /// Validate budget values are non-zero.
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.cpu_millis_per_second == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "cpu_millis_per_second must be > 0".to_string(),
            });
        }
        if self.memory_bytes == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "memory_bytes must be > 0".to_string(),
            });
        }
        if self.io_bytes_per_second == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "io_bytes_per_second must be > 0".to_string(),
            });
        }
        if self.max_inflight_ops == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "max_inflight_ops must be > 0".to_string(),
            });
        }
        Ok(())
    }
}

/// Host runtime configuration for connector embedding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorHostConfig {
    /// Stable host identifier used in operation envelope IDs.
    pub host_id: String,
    /// Current protocol version for connector interactions.
    pub protocol_version: ConnectorProtocolVersion,
    /// Runtime isolation budgets.
    pub budgets: ConnectorRuntimeBudgets,
    /// Startup timeout budget in milliseconds.
    pub startup_timeout_ms: u64,
    /// Expected heartbeat interval in milliseconds.
    pub heartbeat_interval_ms: u64,
    /// Backoff before retry after failures in milliseconds.
    pub failure_backoff_ms: u64,
}

impl Default for ConnectorHostConfig {
    fn default() -> Self {
        Self {
            host_id: "connector-host-0".to_string(),
            protocol_version: ConnectorProtocolVersion::default(),
            budgets: ConnectorRuntimeBudgets::default(),
            startup_timeout_ms: 10_000,
            heartbeat_interval_ms: 1_000,
            failure_backoff_ms: 5_000,
        }
    }
}

impl ConnectorHostConfig {
    /// Validate config values are coherent.
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.host_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "host_id must not be empty".to_string(),
            });
        }
        if self.startup_timeout_ms == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "startup_timeout_ms must be > 0".to_string(),
            });
        }
        if self.heartbeat_interval_ms == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "heartbeat_interval_ms must be > 0".to_string(),
            });
        }
        if self.failure_backoff_ms == 0 {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "failure_backoff_ms must be > 0".to_string(),
            });
        }
        self.budgets.validate()
    }
}

/// Runtime usage snapshot for budget checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorRuntimeUsage {
    pub cpu_millis_in_window: u32,
    pub memory_bytes: u64,
    pub io_bytes_in_window: u64,
    pub inflight_ops: u32,
}

impl ConnectorRuntimeUsage {
    /// Return the first exceeded budget dimension, if any.
    #[must_use]
    pub fn exceeded_dimension(&self, budgets: &ConnectorRuntimeBudgets) -> Option<&'static str> {
        if self.cpu_millis_in_window > budgets.cpu_millis_per_second {
            return Some("cpu_millis_per_second");
        }
        if self.memory_bytes > budgets.memory_bytes {
            return Some("memory_bytes");
        }
        if self.io_bytes_in_window > budgets.io_bytes_per_second {
            return Some("io_bytes_per_second");
        }
        if self.inflight_ops > budgets.max_inflight_ops {
            return Some("max_inflight_ops");
        }
        None
    }
}

/// Concrete failure payload used by degraded/failed states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorFailure {
    pub class: ConnectorFailureClass,
    pub reason_code: String,
    pub observed_at_ms: u64,
}

/// Coarse lifecycle phases for transition records and policy checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorLifecyclePhase {
    Stopped,
    Starting,
    Running,
    Degraded,
    Failed,
}

impl ConnectorLifecyclePhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }
}

impl std::fmt::Display for ConnectorLifecyclePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Full lifecycle state with failure context when degraded/failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorLifecycleState {
    Stopped,
    Starting,
    Running,
    Degraded(ConnectorFailure),
    Failed(ConnectorFailure),
}

impl ConnectorLifecycleState {
    #[must_use]
    pub const fn phase(&self) -> ConnectorLifecyclePhase {
        match self {
            Self::Stopped => ConnectorLifecyclePhase::Stopped,
            Self::Starting => ConnectorLifecyclePhase::Starting,
            Self::Running => ConnectorLifecyclePhase::Running,
            Self::Degraded(_) => ConnectorLifecyclePhase::Degraded,
            Self::Failed(_) => ConnectorLifecyclePhase::Failed,
        }
    }

    #[must_use]
    pub const fn failure(&self) -> Option<&ConnectorFailure> {
        match self {
            Self::Degraded(failure) | Self::Failed(failure) => Some(failure),
            Self::Stopped | Self::Starting | Self::Running => None,
        }
    }
}

/// Startup probe result used to make degraded/failure paths deterministic in tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupProbeResult {
    Healthy,
    Failed {
        class: ConnectorFailureClass,
        reason_code: String,
    },
}

impl StartupProbeResult {
    #[must_use]
    pub const fn healthy() -> Self {
        Self::Healthy
    }

    #[must_use]
    pub fn failed(class: ConnectorFailureClass, reason_code: impl Into<String>) -> Self {
        Self::Failed {
            class,
            reason_code: reason_code.into(),
        }
    }
}

/// Transition record for auditable lifecycle behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorLifecycleTransition {
    pub at_ms: u64,
    pub from: ConnectorLifecyclePhase,
    pub to: ConnectorLifecyclePhase,
    pub reason_code: String,
}

/// Health/liveness projection for operator and machine APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorHealthSnapshot {
    pub host_id: String,
    pub protocol_version: ConnectorProtocolVersion,
    pub phase: ConnectorLifecyclePhase,
    pub is_live: bool,
    pub is_ready: bool,
    pub last_transition_at_ms: u64,
    pub last_heartbeat_at_ms: Option<u64>,
    pub heartbeat_age_ms: Option<u64>,
    pub active_failures: u32,
    pub budgets: ConnectorRuntimeBudgets,
    pub latest_usage: Option<ConnectorRuntimeUsage>,
    pub last_failure: Option<ConnectorFailure>,
}

/// Protocol envelope for connector operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOperationEnvelope {
    pub operation_id: String,
    pub correlation_id: String,
    pub host_id: String,
    pub protocol_version: ConnectorProtocolVersion,
    pub action: String,
    pub issued_at_ms: u64,
}

/// Runtime manager for connector host lifecycle and budget isolation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorHostRuntime {
    config: ConnectorHostConfig,
    state: ConnectorLifecycleState,
    last_transition_at_ms: u64,
    last_heartbeat_at_ms: Option<u64>,
    last_upgrade_at_ms: Option<u64>,
    active_failures: u32,
    operation_seq: u64,
    latest_usage: Option<ConnectorRuntimeUsage>,
    transition_history: VecDeque<ConnectorLifecycleTransition>,
}

impl ConnectorHostRuntime {
    /// Create a new connector host runtime in the `stopped` phase.
    pub fn new(config: ConnectorHostConfig) -> Result<Self, ConnectorHostRuntimeError> {
        config.validate()?;
        Ok(Self {
            config,
            state: ConnectorLifecycleState::Stopped,
            last_transition_at_ms: 0,
            last_heartbeat_at_ms: None,
            last_upgrade_at_ms: None,
            active_failures: 0,
            operation_seq: 0,
            latest_usage: None,
            transition_history: VecDeque::with_capacity(TRANSITION_HISTORY_CAPACITY),
        })
    }

    /// Current lifecycle state.
    #[must_use]
    pub const fn state(&self) -> &ConnectorLifecycleState {
        &self.state
    }

    /// Runtime configuration.
    #[must_use]
    pub const fn config(&self) -> &ConnectorHostConfig {
        &self.config
    }

    /// Transition history (oldest to newest).
    #[must_use]
    pub fn transition_history(&self) -> Vec<ConnectorLifecycleTransition> {
        self.transition_history.iter().cloned().collect()
    }

    /// Start the host with a healthy startup probe.
    pub fn start(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        self.start_with_probe(now_ms, StartupProbeResult::Healthy)
    }

    /// Start the host with an explicit startup probe result.
    pub fn start_with_probe(
        &mut self,
        now_ms: u64,
        probe: StartupProbeResult,
    ) -> Result<(), ConnectorHostRuntimeError> {
        let from = self.state.phase();
        if matches!(from, ConnectorLifecyclePhase::Starting | ConnectorLifecyclePhase::Running) {
            return Err(ConnectorHostRuntimeError::InvalidTransition {
                from,
                to: ConnectorLifecyclePhase::Starting,
                reason: "host is already starting or running".to_string(),
            });
        }

        self.transition(now_ms, ConnectorLifecycleState::Starting, "lifecycle.start.requested");
        match probe {
            StartupProbeResult::Healthy => {
                self.transition(now_ms, ConnectorLifecycleState::Running, "lifecycle.start.ready");
                self.last_heartbeat_at_ms = Some(now_ms);
                Ok(())
            }
            StartupProbeResult::Failed { class, reason_code } => {
                ensure_reason_code(&reason_code)?;
                self.active_failures = self.active_failures.saturating_add(1);
                let failure = ConnectorFailure {
                    class,
                    reason_code: reason_code.clone(),
                    observed_at_ms: now_ms,
                };
                self.transition(
                    now_ms,
                    ConnectorLifecycleState::Failed(failure),
                    "lifecycle.start.failed",
                );
                Err(ConnectorHostRuntimeError::StartupProbeFailed { class, reason_code })
            }
        }
    }

    /// Stop the host.
    pub fn stop(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        if self.state.phase() == ConnectorLifecyclePhase::Stopped {
            return Err(ConnectorHostRuntimeError::InvalidTransition {
                from: ConnectorLifecyclePhase::Stopped,
                to: ConnectorLifecyclePhase::Stopped,
                reason: "host is already stopped".to_string(),
            });
        }
        self.transition(now_ms, ConnectorLifecycleState::Stopped, "lifecycle.stop.requested");
        self.last_heartbeat_at_ms = None;
        self.latest_usage = None;
        Ok(())
    }

    /// Restart the host using a healthy startup probe.
    pub fn restart(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        self.restart_with_probe(now_ms, StartupProbeResult::Healthy)
    }

    /// Restart the host with an explicit startup probe result.
    pub fn restart_with_probe(
        &mut self,
        now_ms: u64,
        probe: StartupProbeResult,
    ) -> Result<(), ConnectorHostRuntimeError> {
        if self.state.phase() != ConnectorLifecyclePhase::Stopped {
            self.stop(now_ms)?;
        }
        self.start_with_probe(now_ms, probe)
    }

    /// Upgrade protocol version and restart if host is currently live.
    pub fn upgrade_and_restart(
        &mut self,
        now_ms: u64,
        new_version: ConnectorProtocolVersion,
        probe: StartupProbeResult,
    ) -> Result<(), ConnectorHostRuntimeError> {
        if new_version <= self.config.protocol_version {
            return Err(ConnectorHostRuntimeError::ProtocolUpgradeRejected {
                reason: format!(
                    "new version {new_version} must be greater than current {}",
                    self.config.protocol_version
                ),
            });
        }

        let was_live = matches!(
            self.state.phase(),
            ConnectorLifecyclePhase::Starting
                | ConnectorLifecyclePhase::Running
                | ConnectorLifecyclePhase::Degraded
        );
        if was_live {
            self.stop(now_ms)?;
        }
        self.config.protocol_version = new_version;
        self.last_upgrade_at_ms = Some(now_ms);
        self.transition(now_ms, self.state.clone(), "lifecycle.upgrade.applied");
        if was_live {
            self.start_with_probe(now_ms, probe)?;
        }
        Ok(())
    }

    /// Record a heartbeat from the connector host.
    pub fn record_heartbeat(&mut self, now_ms: u64) -> Result<(), ConnectorHostRuntimeError> {
        match self.state.phase() {
            ConnectorLifecyclePhase::Running | ConnectorLifecyclePhase::Degraded => {
                self.last_heartbeat_at_ms = Some(now_ms);
                Ok(())
            }
            phase => Err(ConnectorHostRuntimeError::HostNotRunnable { phase }),
        }
    }

    /// Observe runtime usage and enforce configured budgets.
    pub fn observe_usage(
        &mut self,
        now_ms: u64,
        usage: ConnectorRuntimeUsage,
    ) -> Result<(), ConnectorHostRuntimeError> {
        self.latest_usage = Some(usage);
        if let Some(dimension) = usage.exceeded_dimension(&self.config.budgets) {
            self.active_failures = self.active_failures.saturating_add(1);
            let failure = ConnectorFailure {
                class: ConnectorFailureClass::Quota,
                reason_code: format!("budget_exceeded.{dimension}"),
                observed_at_ms: now_ms,
            };
            self.transition(
                now_ms,
                ConnectorLifecycleState::Degraded(failure),
                "lifecycle.degraded.budget_exceeded",
            );
            return Err(ConnectorHostRuntimeError::BudgetExceeded {
                dimension: dimension.to_string(),
            });
        }

        if self.state.phase() == ConnectorLifecyclePhase::Degraded {
            self.transition(
                now_ms,
                ConnectorLifecycleState::Running,
                "lifecycle.degraded.recovered",
            );
        }

        Ok(())
    }

    /// Mark an explicit runtime failure.
    pub fn mark_failure(
        &mut self,
        now_ms: u64,
        class: ConnectorFailureClass,
        reason_code: impl Into<String>,
    ) -> Result<(), ConnectorHostRuntimeError> {
        let reason_code = reason_code.into();
        ensure_reason_code(&reason_code)?;
        self.active_failures = self.active_failures.saturating_add(1);
        let failure = ConnectorFailure {
            class,
            reason_code,
            observed_at_ms: now_ms,
        };
        self.transition(
            now_ms,
            ConnectorLifecycleState::Failed(failure),
            "lifecycle.failed",
        );
        Ok(())
    }

    /// Build a versioned operation envelope with monotonic operation ID.
    pub fn build_operation_envelope(
        &mut self,
        now_ms: u64,
        action: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Result<ConnectorOperationEnvelope, ConnectorHostRuntimeError> {
        if self.state.phase() != ConnectorLifecyclePhase::Running {
            return Err(ConnectorHostRuntimeError::HostNotRunnable {
                phase: self.state.phase(),
            });
        }

        let action = action.into();
        if action.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "action must not be empty".to_string(),
            });
        }

        let correlation_id = correlation_id.into();
        if correlation_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "correlation_id must not be empty".to_string(),
            });
        }

        self.operation_seq = self
            .operation_seq
            .checked_add(1)
            .ok_or_else(|| ConnectorHostRuntimeError::InvalidConfig {
                reason: "operation sequence overflow".to_string(),
            })?;
        let operation_id = format!("{}-op-{:016x}", self.config.host_id, self.operation_seq);

        Ok(ConnectorOperationEnvelope {
            operation_id,
            correlation_id,
            host_id: self.config.host_id.clone(),
            protocol_version: self.config.protocol_version,
            action,
            issued_at_ms: now_ms,
        })
    }

    /// Render the current health/liveness snapshot.
    #[must_use]
    pub fn health_snapshot(&self, now_ms: u64) -> ConnectorHealthSnapshot {
        let heartbeat_age_ms = self
            .last_heartbeat_at_ms
            .map(|last| now_ms.saturating_sub(last));
        let live_deadline_ms = self.config.heartbeat_interval_ms.saturating_mul(3);
        let is_live = matches!(
            self.state.phase(),
            ConnectorLifecyclePhase::Running | ConnectorLifecyclePhase::Degraded
        ) && heartbeat_age_ms.is_some_and(|age| age <= live_deadline_ms);
        let is_ready = self.state.phase() == ConnectorLifecyclePhase::Running
            && is_live
            && self
                .latest_usage
                .is_none_or(|usage| usage.exceeded_dimension(&self.config.budgets).is_none());

        ConnectorHealthSnapshot {
            host_id: self.config.host_id.clone(),
            protocol_version: self.config.protocol_version,
            phase: self.state.phase(),
            is_live,
            is_ready,
            last_transition_at_ms: self.last_transition_at_ms,
            last_heartbeat_at_ms: self.last_heartbeat_at_ms,
            heartbeat_age_ms,
            active_failures: self.active_failures,
            budgets: self.config.budgets,
            latest_usage: self.latest_usage,
            last_failure: self.state.failure().cloned(),
        }
    }

    fn transition(
        &mut self,
        at_ms: u64,
        to_state: ConnectorLifecycleState,
        reason_code: &str,
    ) {
        let transition = ConnectorLifecycleTransition {
            at_ms,
            from: self.state.phase(),
            to: to_state.phase(),
            reason_code: reason_code.to_string(),
        };
        self.state = to_state;
        self.last_transition_at_ms = at_ms;
        self.transition_history.push_back(transition);
        while self.transition_history.len() > TRANSITION_HISTORY_CAPACITY {
            self.transition_history.pop_front();
        }
    }
}

fn ensure_reason_code(reason_code: &str) -> Result<(), ConnectorHostRuntimeError> {
    if reason_code.trim().is_empty() {
        return Err(ConnectorHostRuntimeError::InvalidConfig {
            reason: "reason_code must not be empty".to_string(),
        });
    }
    Ok(())
}

/// Deterministic connector-runtime error taxonomy.
#[derive(Debug, Error, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConnectorHostRuntimeError {
    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },
    #[error("invalid lifecycle transition {from} -> {to}: {reason}")]
    InvalidTransition {
        from: ConnectorLifecyclePhase,
        to: ConnectorLifecyclePhase,
        reason: String,
    },
    #[error("startup probe failed ({class}): {reason_code}")]
    StartupProbeFailed {
        class: ConnectorFailureClass,
        reason_code: String,
    },
    #[error("resource budget exceeded: {dimension}")]
    BudgetExceeded { dimension: String },
    #[error("host is not runnable in phase {phase}")]
    HostNotRunnable { phase: ConnectorLifecyclePhase },
    #[error("protocol upgrade rejected: {reason}")]
    ProtocolUpgradeRejected { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_within_budget() -> ConnectorRuntimeUsage {
        ConnectorRuntimeUsage {
            cpu_millis_in_window: 120,
            memory_bytes: 128 * 1024 * 1024,
            io_bytes_in_window: 512 * 1024,
            inflight_ops: 4,
        }
    }

    #[test]
    fn connector_host_runtime_start_stop_restart_happy_path() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);

        runtime.stop(200).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Stopped);

        runtime.restart(300).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_startup_probe_failure_sets_failed_state() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        let err = runtime
            .start_with_probe(
                100,
                StartupProbeResult::failed(ConnectorFailureClass::Network, "dial_failed"),
            )
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::StartupProbeFailed {
                class: ConnectorFailureClass::Network,
                reason_code: "dial_failed".to_string(),
            }
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Failed);
        assert_eq!(runtime.health_snapshot(150).active_failures, 1);
    }

    #[test]
    fn connector_host_runtime_budget_exceedance_forces_degraded_state() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();

        let err = runtime
            .observe_usage(
                120,
                ConnectorRuntimeUsage {
                    cpu_millis_in_window: 900,
                    ..usage_within_budget()
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::BudgetExceeded {
                dimension: "cpu_millis_per_second".to_string(),
            }
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Degraded);
    }

    #[test]
    fn connector_host_runtime_degraded_recovers_when_usage_recovers() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        let _ = runtime.observe_usage(
            110,
            ConnectorRuntimeUsage {
                memory_bytes: 1024 * 1024 * 1024,
                ..usage_within_budget()
            },
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Degraded);

        runtime.observe_usage(140, usage_within_budget()).unwrap();
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_upgrade_and_restart_updates_protocol_version() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        runtime
            .upgrade_and_restart(200, ConnectorProtocolVersion::new(1, 1, 0), StartupProbeResult::healthy())
            .unwrap();

        assert_eq!(runtime.config().protocol_version, ConnectorProtocolVersion::new(1, 1, 0));
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Running);
    }

    #[test]
    fn connector_host_runtime_operation_envelope_monotonic_and_versioned() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(105, usage_within_budget()).unwrap();

        let op1 = runtime
            .build_operation_envelope(110, "connector.invoke", "corr-1")
            .unwrap();
        let op2 = runtime
            .build_operation_envelope(120, "connector.invoke", "corr-2")
            .unwrap();

        assert!(op1.operation_id < op2.operation_id);
        assert_eq!(op1.protocol_version, ConnectorProtocolVersion::new(1, 0, 0));
        assert_eq!(op2.protocol_version, ConnectorProtocolVersion::new(1, 0, 0));
    }

    #[test]
    fn connector_host_runtime_health_liveness_heartbeat_timeout() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(1_000).unwrap();
        runtime.record_heartbeat(1_500).unwrap();
        let healthy = runtime.health_snapshot(3_000);
        assert!(healthy.is_live);

        // heartbeat timeout = 3 * 1000ms = 3000ms; age here is 3501ms.
        let stale = runtime.health_snapshot(5_001);
        assert!(!stale.is_live);
        assert!(!stale.is_ready);
    }

    #[test]
    fn connector_host_runtime_transition_history_is_bounded() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        for i in 0..100 {
            runtime.transition(i, ConnectorLifecycleState::Stopped, "test.transition");
        }
        assert_eq!(runtime.transition_history().len(), TRANSITION_HISTORY_CAPACITY);
    }

    #[test]
    fn connector_host_runtime_config_validation_rejects_zero_budget() {
        let mut config = ConnectorHostConfig::default();
        config.budgets.max_inflight_ops = 0;
        let err = ConnectorHostRuntime::new(config).unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::InvalidConfig {
                reason: "max_inflight_ops must be > 0".to_string(),
            }
        );
    }
}
