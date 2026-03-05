//! Connector host runtime lifecycle and protocol envelopes.
//!
//! This module provides a deterministic, testable host-runtime core for
//! connector-fabric embedding. It intentionally avoids side effects and uses
//! caller-provided timestamps so lifecycle behavior is reproducible in tests.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use thiserror::Error;

const TRANSITION_HISTORY_CAPACITY: usize = 64;
const SANDBOX_DECISION_HISTORY_CAPACITY: usize = 128;

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

/// Capability gates available to connector operations inside sandbox zones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorCapability {
    Invoke,
    ReadState,
    StreamEvents,
    FilesystemRead,
    FilesystemWrite,
    NetworkEgress,
    SecretBroker,
    ProcessExec,
}

impl ConnectorCapability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Invoke => "invoke",
            Self::ReadState => "read_state",
            Self::StreamEvents => "stream_events",
            Self::FilesystemRead => "filesystem_read",
            Self::FilesystemWrite => "filesystem_write",
            Self::NetworkEgress => "network_egress",
            Self::SecretBroker => "secret_broker",
            Self::ProcessExec => "process_exec",
        }
    }
}

impl std::fmt::Display for ConnectorCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Explicit capability envelope and target constraints for connector execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorCapabilityEnvelope {
    pub allowed_capabilities: Vec<ConnectorCapability>,
    pub filesystem_read_prefixes: Vec<String>,
    pub filesystem_write_prefixes: Vec<String>,
    pub network_allow_hosts: Vec<String>,
    pub allowed_exec_commands: Vec<String>,
}

impl Default for ConnectorCapabilityEnvelope {
    fn default() -> Self {
        Self {
            allowed_capabilities: vec![
                ConnectorCapability::Invoke,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
            ],
            filesystem_read_prefixes: Vec::new(),
            filesystem_write_prefixes: Vec::new(),
            network_allow_hosts: Vec::new(),
            allowed_exec_commands: Vec::new(),
        }
    }
}

impl ConnectorCapabilityEnvelope {
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.allowed_capabilities.is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "allowed_capabilities must not be empty".to_string(),
            });
        }
        for prefix in &self.filesystem_read_prefixes {
            if prefix.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "filesystem_read_prefixes must not contain empty values".to_string(),
                });
            }
        }
        for prefix in &self.filesystem_write_prefixes {
            if prefix.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "filesystem_write_prefixes must not contain empty values".to_string(),
                });
            }
        }
        for host in &self.network_allow_hosts {
            if host.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "network_allow_hosts must not contain empty values".to_string(),
                });
            }
        }
        for command in &self.allowed_exec_commands {
            if command.trim().is_empty() {
                return Err(ConnectorHostRuntimeError::InvalidConfig {
                    reason: "allowed_exec_commands must not contain empty values".to_string(),
                });
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn allows_capability(&self, capability: ConnectorCapability) -> bool {
        self.allowed_capabilities.contains(&capability)
    }

    #[must_use]
    pub fn allows_target(&self, capability: ConnectorCapability, target: Option<&str>) -> bool {
        match capability {
            ConnectorCapability::FilesystemRead => target.is_some_and(|path| {
                self.filesystem_read_prefixes
                    .iter()
                    .any(|prefix| path_is_within_prefix(path, prefix))
            }),
            ConnectorCapability::FilesystemWrite => target.is_some_and(|path| {
                self.filesystem_write_prefixes
                    .iter()
                    .any(|prefix| path_is_within_prefix(path, prefix))
            }),
            ConnectorCapability::NetworkEgress => target.is_some_and(|host| {
                self.network_allow_hosts.iter().any(|allowed| {
                    if allowed.starts_with("*.") {
                        host.ends_with(&allowed[1..])
                    } else {
                        host == allowed
                    }
                })
            }),
            ConnectorCapability::ProcessExec => target.is_some_and(|command| {
                self.allowed_exec_commands
                    .iter()
                    .any(|allowed| allowed == command)
            }),
            ConnectorCapability::Invoke
            | ConnectorCapability::ReadState
            | ConnectorCapability::StreamEvents
            | ConnectorCapability::SecretBroker => true,
        }
    }
}

fn normalize_absolute_path(path: &str) -> Option<Vec<String>> {
    use std::path::Component;

    let mut saw_root = false;
    let mut parts: Vec<String> = Vec::new();

    for component in std::path::Path::new(path).components() {
        match component {
            Component::RootDir => saw_root = true,
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Prefix(_) => return None,
        }
    }

    if !saw_root {
        return None;
    }

    Some(parts)
}

fn path_is_within_prefix(path: &str, prefix: &str) -> bool {
    let Some(path_parts) = normalize_absolute_path(path) else {
        return false;
    };
    let Some(prefix_parts) = normalize_absolute_path(prefix) else {
        return false;
    };

    if prefix_parts.len() > path_parts.len() {
        return false;
    }

    path_parts
        .iter()
        .zip(prefix_parts.iter())
        .all(|(candidate, required)| candidate == required)
}

/// Sandbox zone boundary for connector runtime operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSandboxZone {
    pub zone_id: String,
    pub fail_closed: bool,
    pub capability_envelope: ConnectorCapabilityEnvelope,
}

impl Default for ConnectorSandboxZone {
    fn default() -> Self {
        Self {
            zone_id: "zone.default".to_string(),
            fail_closed: true,
            capability_envelope: ConnectorCapabilityEnvelope::default(),
        }
    }
}

impl ConnectorSandboxZone {
    pub fn validate(&self) -> Result<(), ConnectorHostRuntimeError> {
        if self.zone_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "sandbox.zone_id must not be empty".to_string(),
            });
        }
        self.capability_envelope.validate()
    }
}

/// Auditable sandbox decision for each operation authorization attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorSandboxDecision {
    pub decision_id: String,
    pub at_ms: u64,
    pub zone_id: String,
    pub action: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
    pub allowed: bool,
    pub reason_code: String,
}

/// Input used for sandbox authorization of connector operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOperationRequest {
    pub action: String,
    pub correlation_id: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
}

impl ConnectorOperationRequest {
    #[must_use]
    pub fn new(
        action: impl Into<String>,
        correlation_id: impl Into<String>,
        capability: ConnectorCapability,
    ) -> Self {
        Self {
            action: action.into(),
            correlation_id: correlation_id.into(),
            capability,
            target: None,
        }
    }

    #[must_use]
    pub fn with_target(mut self, target: impl Into<String>) -> Self {
        self.target = Some(target.into());
        self
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
    /// Sandbox zone and capability envelope constraints for connector execution.
    pub sandbox: ConnectorSandboxZone,
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
            sandbox: ConnectorSandboxZone::default(),
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
        self.sandbox.validate()?;
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
    pub sandbox_zone_id: String,
    pub budgets: ConnectorRuntimeBudgets,
    pub latest_usage: Option<ConnectorRuntimeUsage>,
    pub last_failure: Option<ConnectorFailure>,
    pub last_sandbox_decision: Option<ConnectorSandboxDecision>,
}

/// Protocol envelope for connector operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorOperationEnvelope {
    pub operation_id: String,
    pub correlation_id: String,
    pub host_id: String,
    pub zone_id: String,
    pub protocol_version: ConnectorProtocolVersion,
    pub action: String,
    pub capability: ConnectorCapability,
    pub target: Option<String>,
    pub decision_id: String,
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
    sandbox_decision_seq: u64,
    latest_usage: Option<ConnectorRuntimeUsage>,
    transition_history: VecDeque<ConnectorLifecycleTransition>,
    sandbox_decisions: VecDeque<ConnectorSandboxDecision>,
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
            sandbox_decision_seq: 0,
            latest_usage: None,
            transition_history: VecDeque::with_capacity(TRANSITION_HISTORY_CAPACITY),
            sandbox_decisions: VecDeque::with_capacity(SANDBOX_DECISION_HISTORY_CAPACITY),
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

    /// Sandbox decision history (oldest to newest).
    #[must_use]
    pub fn sandbox_decision_history(&self) -> Vec<ConnectorSandboxDecision> {
        self.sandbox_decisions.iter().cloned().collect()
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
        if matches!(
            from,
            ConnectorLifecyclePhase::Starting | ConnectorLifecyclePhase::Running
        ) {
            return Err(ConnectorHostRuntimeError::InvalidTransition {
                from,
                to: ConnectorLifecyclePhase::Starting,
                reason: "host is already starting or running".to_string(),
            });
        }

        self.transition(
            now_ms,
            ConnectorLifecycleState::Starting,
            "lifecycle.start.requested",
        );
        match probe {
            StartupProbeResult::Healthy => {
                self.transition(
                    now_ms,
                    ConnectorLifecycleState::Running,
                    "lifecycle.start.ready",
                );
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
        self.transition(
            now_ms,
            ConnectorLifecycleState::Stopped,
            "lifecycle.stop.requested",
        );
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
        let action = action.into();
        let correlation_id = correlation_id.into();
        let capability = infer_capability_from_action(&action);
        self.authorize_operation(
            now_ms,
            ConnectorOperationRequest::new(action, correlation_id, capability),
        )
    }

    /// Authorize a connector operation against sandbox zone and capability envelope.
    pub fn authorize_operation(
        &mut self,
        now_ms: u64,
        request: ConnectorOperationRequest,
    ) -> Result<ConnectorOperationEnvelope, ConnectorHostRuntimeError> {
        if self.state.phase() != ConnectorLifecyclePhase::Running {
            return Err(ConnectorHostRuntimeError::HostNotRunnable {
                phase: self.state.phase(),
            });
        }

        if request.action.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "action must not be empty".to_string(),
            });
        }

        if request.correlation_id.trim().is_empty() {
            return Err(ConnectorHostRuntimeError::InvalidConfig {
                reason: "correlation_id must not be empty".to_string(),
            });
        }

        let capability_allowed = self
            .config
            .sandbox
            .capability_envelope
            .allows_capability(request.capability);
        let target_allowed = self
            .config
            .sandbox
            .capability_envelope
            .allows_target(request.capability, request.target.as_deref());

        self.sandbox_decision_seq = self.sandbox_decision_seq.checked_add(1).ok_or_else(|| {
            ConnectorHostRuntimeError::InvalidConfig {
                reason: "sandbox decision sequence overflow".to_string(),
            }
        })?;
        let decision_id = format!(
            "{}-sd-{:016x}",
            self.config.host_id, self.sandbox_decision_seq
        );

        if !capability_allowed || !target_allowed {
            let reason_code = if !capability_allowed {
                format!("sandbox.denied.capability.{}", request.capability)
            } else {
                format!("sandbox.denied.target.{}", request.capability)
            };
            let decision = ConnectorSandboxDecision {
                decision_id: decision_id.clone(),
                at_ms: now_ms,
                zone_id: self.config.sandbox.zone_id.clone(),
                action: request.action.clone(),
                capability: request.capability,
                target: request.target.clone(),
                allowed: false,
                reason_code: reason_code.clone(),
            };
            self.record_sandbox_decision(decision);

            if self.config.sandbox.fail_closed {
                self.active_failures = self.active_failures.saturating_add(1);
                let failure = ConnectorFailure {
                    class: ConnectorFailureClass::Policy,
                    reason_code: reason_code.clone(),
                    observed_at_ms: now_ms,
                };
                self.transition(
                    now_ms,
                    ConnectorLifecycleState::Failed(failure),
                    "lifecycle.failed.sandbox_violation",
                );
            }

            return Err(ConnectorHostRuntimeError::SandboxViolation {
                zone_id: self.config.sandbox.zone_id.clone(),
                capability: request.capability,
                reason_code,
            });
        }

        let allowed_decision = ConnectorSandboxDecision {
            decision_id: decision_id.clone(),
            at_ms: now_ms,
            zone_id: self.config.sandbox.zone_id.clone(),
            action: request.action.clone(),
            capability: request.capability,
            target: request.target.clone(),
            allowed: true,
            reason_code: "sandbox.allowed".to_string(),
        };
        self.record_sandbox_decision(allowed_decision);

        self.operation_seq = self.operation_seq.checked_add(1).ok_or_else(|| {
            ConnectorHostRuntimeError::InvalidConfig {
                reason: "operation sequence overflow".to_string(),
            }
        })?;
        let operation_id = format!("{}-op-{:016x}", self.config.host_id, self.operation_seq);

        Ok(ConnectorOperationEnvelope {
            operation_id,
            correlation_id: request.correlation_id,
            host_id: self.config.host_id.clone(),
            zone_id: self.config.sandbox.zone_id.clone(),
            protocol_version: self.config.protocol_version,
            action: request.action,
            capability: request.capability,
            target: request.target,
            decision_id,
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
            sandbox_zone_id: self.config.sandbox.zone_id.clone(),
            budgets: self.config.budgets,
            latest_usage: self.latest_usage,
            last_failure: self.state.failure().cloned(),
            last_sandbox_decision: self.sandbox_decisions.back().cloned(),
        }
    }

    fn record_sandbox_decision(&mut self, decision: ConnectorSandboxDecision) {
        self.sandbox_decisions.push_back(decision);
        while self.sandbox_decisions.len() > SANDBOX_DECISION_HISTORY_CAPACITY {
            self.sandbox_decisions.pop_front();
        }
    }

    fn transition(&mut self, at_ms: u64, to_state: ConnectorLifecycleState, reason_code: &str) {
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

fn infer_capability_from_action(action: &str) -> ConnectorCapability {
    if action.contains("stream") {
        return ConnectorCapability::StreamEvents;
    }
    if action.contains("state") || action.contains("status") || action.contains("ping") {
        return ConnectorCapability::ReadState;
    }
    ConnectorCapability::Invoke
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
    #[error("sandbox violation in zone {zone_id} for capability {capability}: {reason_code}")]
    SandboxViolation {
        zone_id: String,
        capability: ConnectorCapability,
        reason_code: String,
    },
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
            .upgrade_and_restart(
                200,
                ConnectorProtocolVersion::new(1, 1, 0),
                StartupProbeResult::healthy(),
            )
            .unwrap();

        assert_eq!(
            runtime.config().protocol_version,
            ConnectorProtocolVersion::new(1, 1, 0)
        );
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
        assert_eq!(op1.zone_id, "zone.default");
        assert_eq!(op1.capability, ConnectorCapability::Invoke);
        assert!(op1.decision_id < op2.decision_id);
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
        assert_eq!(
            runtime.transition_history().len(),
            TRANSITION_HISTORY_CAPACITY
        );
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

    #[test]
    fn connector_host_runtime_sandbox_denies_missing_capability_fail_closed() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.capability_envelope.allowed_capabilities =
            vec![ConnectorCapability::ReadState];
        let mut runtime = ConnectorHostRuntime::new(config).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(110, usage_within_budget()).unwrap();

        let err = runtime
            .authorize_operation(
                120,
                ConnectorOperationRequest::new(
                    "connector.invoke",
                    "corr-deny-capability",
                    ConnectorCapability::Invoke,
                ),
            )
            .unwrap_err();
        assert_eq!(
            err,
            ConnectorHostRuntimeError::SandboxViolation {
                zone_id: "zone.default".to_string(),
                capability: ConnectorCapability::Invoke,
                reason_code: "sandbox.denied.capability.invoke".to_string(),
            }
        );
        assert_eq!(runtime.state().phase(), ConnectorLifecyclePhase::Failed);
        assert_eq!(runtime.sandbox_decision_history().len(), 1);
        assert!(
            !runtime
                .sandbox_decision_history()
                .last()
                .expect("expected denial decision")
                .allowed
        );
    }

    #[test]
    fn connector_host_runtime_sandbox_enforces_target_allowlists() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.capability_envelope.allowed_capabilities = vec![
            ConnectorCapability::Invoke,
            ConnectorCapability::NetworkEgress,
        ];
        config.sandbox.capability_envelope.network_allow_hosts =
            vec!["api.frankenterm.dev".to_string()];
        let mut runtime = ConnectorHostRuntime::new(config).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(105, usage_within_budget()).unwrap();

        let denied = runtime.authorize_operation(
            110,
            ConnectorOperationRequest::new(
                "connector.network.call",
                "corr-net-deny",
                ConnectorCapability::NetworkEgress,
            )
            .with_target("evil.example.com"),
        );
        assert_eq!(
            denied.unwrap_err(),
            ConnectorHostRuntimeError::SandboxViolation {
                zone_id: "zone.default".to_string(),
                capability: ConnectorCapability::NetworkEgress,
                reason_code: "sandbox.denied.target.network_egress".to_string(),
            }
        );

        runtime
            .restart_with_probe(120, StartupProbeResult::healthy())
            .unwrap();
        runtime.observe_usage(121, usage_within_budget()).unwrap();
        let allowed = runtime
            .authorize_operation(
                125,
                ConnectorOperationRequest::new(
                    "connector.network.call",
                    "corr-net-allow",
                    ConnectorCapability::NetworkEgress,
                )
                .with_target("api.frankenterm.dev"),
            )
            .unwrap();
        assert_eq!(allowed.target.as_deref(), Some("api.frankenterm.dev"));
        assert_eq!(allowed.capability, ConnectorCapability::NetworkEgress);

        let decisions = runtime.sandbox_decision_history();
        assert_eq!(decisions.len(), 2);
        assert_eq!(
            decisions.iter().filter(|decision| decision.allowed).count(),
            1
        );
    }

    #[test]
    fn connector_host_runtime_sandbox_filesystem_prefix_is_boundary_safe() {
        let mut config = ConnectorHostConfig::default();
        config.sandbox.fail_closed = false;
        config.sandbox.capability_envelope.allowed_capabilities = vec![
            ConnectorCapability::Invoke,
            ConnectorCapability::FilesystemRead,
        ];
        config.sandbox.capability_envelope.filesystem_read_prefixes =
            vec!["/workspace/safe".to_string()];
        let mut runtime = ConnectorHostRuntime::new(config).unwrap();
        runtime.start(100).unwrap();
        runtime.observe_usage(105, usage_within_budget()).unwrap();

        let allowed = runtime
            .authorize_operation(
                110,
                ConnectorOperationRequest::new(
                    "connector.fs.read",
                    "corr-fs-allow",
                    ConnectorCapability::FilesystemRead,
                )
                .with_target("/workspace/safe/file.txt"),
            )
            .unwrap();
        assert_eq!(allowed.target.as_deref(), Some("/workspace/safe/file.txt"));

        let boundary_bypass = runtime
            .authorize_operation(
                120,
                ConnectorOperationRequest::new(
                    "connector.fs.read",
                    "corr-fs-boundary",
                    ConnectorCapability::FilesystemRead,
                )
                .with_target("/workspace/safe2/file.txt"),
            )
            .unwrap_err();
        assert!(matches!(
            boundary_bypass,
            ConnectorHostRuntimeError::SandboxViolation { .. }
        ));

        let traversal_bypass = runtime
            .authorize_operation(
                130,
                ConnectorOperationRequest::new(
                    "connector.fs.read",
                    "corr-fs-traversal",
                    ConnectorCapability::FilesystemRead,
                )
                .with_target("/workspace/safe/../secrets.txt"),
            )
            .unwrap_err();
        assert!(matches!(
            traversal_bypass,
            ConnectorHostRuntimeError::SandboxViolation { .. }
        ));
    }

    #[test]
    fn connector_host_runtime_sandbox_decision_history_is_bounded() {
        let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
        runtime.start(1).unwrap();
        runtime.observe_usage(2, usage_within_budget()).unwrap();

        for index in 0..200_u64 {
            let _ = runtime.authorize_operation(
                10 + index,
                ConnectorOperationRequest::new(
                    format!("connector.invoke.{index}"),
                    format!("corr-{index}"),
                    ConnectorCapability::Invoke,
                ),
            );
        }
        assert_eq!(
            runtime.sandbox_decision_history().len(),
            SANDBOX_DECISION_HISTORY_CAPACITY
        );
    }
}
