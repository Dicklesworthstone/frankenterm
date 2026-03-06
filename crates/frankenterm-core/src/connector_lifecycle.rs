//! Connector lifecycle manager: install, update, enable, disable, restart.
//!
//! Orchestrates the full lifecycle of connectors from initial installation through
//! version upgrades, runtime enable/disable, and graceful restart with rollback
//! support. Sits between the registry (package verification) and the host runtime
//! (execution environment).
//!
//! Part of ft-3681t.5.4.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connector_host_runtime::{ConnectorCapability, ConnectorLifecyclePhase};
use crate::connector_registry::{
    ConnectorManifest, ConnectorRegistryError, TrustLevel, TrustPolicy,
};

// =============================================================================
// Lifecycle errors
// =============================================================================

/// Lifecycle-specific errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorLifecycleError {
    #[error("connector not installed: {connector_id}")]
    NotInstalled { connector_id: String },

    #[error("connector already installed: {connector_id} v{version}")]
    AlreadyInstalled {
        connector_id: String,
        version: String,
    },

    #[error("invalid state transition: {connector_id} {from} -> {to}")]
    InvalidTransition {
        connector_id: String,
        from: String,
        to: String,
    },

    #[error("upgrade precondition failed for {connector_id}: {reason}")]
    UpgradePreconditionFailed {
        connector_id: String,
        reason: String,
    },

    #[error("rollback failed for {connector_id}: {reason}")]
    RollbackFailed {
        connector_id: String,
        reason: String,
    },

    #[error("restart limit exceeded for {connector_id}: {count} in {window_secs}s")]
    RestartLimitExceeded {
        connector_id: String,
        count: u32,
        window_secs: u64,
    },

    #[error("registry error: {0}")]
    Registry(#[from] ConnectorRegistryError),

    #[error("lifecycle operation timed out for {connector_id}")]
    OperationTimeout { connector_id: String },
}

// =============================================================================
// Lifecycle intent (user/operator commands)
// =============================================================================

/// An intent describing a desired lifecycle operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
pub enum LifecycleIntent {
    Install {
        manifest: ConnectorManifest,
    },
    Update {
        connector_id: String,
        manifest: ConnectorManifest,
    },
    Enable {
        connector_id: String,
    },
    Disable {
        connector_id: String,
        reason: String,
    },
    Restart {
        connector_id: String,
    },
    Uninstall {
        connector_id: String,
    },
    Rollback {
        connector_id: String,
    },
}

impl LifecycleIntent {
    /// Extract the connector_id that this intent targets.
    #[must_use]
    pub fn connector_id(&self) -> &str {
        match self {
            Self::Install { manifest } => &manifest.package_id,
            Self::Update { connector_id, .. }
            | Self::Enable { connector_id }
            | Self::Disable { connector_id, .. }
            | Self::Restart { connector_id }
            | Self::Uninstall { connector_id }
            | Self::Rollback { connector_id } => connector_id,
        }
    }

    /// Human-readable operation name.
    #[must_use]
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Install { .. } => "install",
            Self::Update { .. } => "update",
            Self::Enable { .. } => "enable",
            Self::Disable { .. } => "disable",
            Self::Restart { .. } => "restart",
            Self::Uninstall { .. } => "uninstall",
            Self::Rollback { .. } => "rollback",
        }
    }
}

// =============================================================================
// Managed connector state
// =============================================================================

/// Administrative state (distinct from runtime phase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminState {
    /// Installed and enabled — eligible to run.
    Enabled,
    /// Installed but administratively disabled.
    Disabled,
    /// Installed, currently upgrading to a new version.
    Upgrading,
    /// Uninstalling (drain + cleanup in progress).
    Uninstalling,
}

impl AdminState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Upgrading => "upgrading",
            Self::Uninstalling => "uninstalling",
        }
    }

    pub const fn can_start(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

impl fmt::Display for AdminState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Full state of a managed connector installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedConnector {
    /// Package ID (unique key).
    pub connector_id: String,
    /// Current installed version.
    pub version: String,
    /// Display name from manifest.
    pub display_name: String,
    /// Administrative state.
    pub admin_state: AdminState,
    /// Runtime lifecycle phase (mirrors host runtime).
    pub runtime_phase: ConnectorLifecyclePhase,
    /// Trust level from last policy evaluation.
    pub trust_level: TrustLevel,
    /// Capabilities granted.
    pub granted_capabilities: Vec<ConnectorCapability>,
    /// Installation timestamp (unix ms).
    pub installed_at_ms: u64,
    /// Last state change timestamp.
    pub last_transition_at_ms: u64,
    /// Previous version (for rollback).
    pub previous_version: Option<String>,
    /// Previous manifest snapshot (for rollback).
    pub rollback_manifest: Option<ConnectorManifest>,
    /// Restart counter in current window.
    pub restart_count: u32,
    /// Start of current restart tracking window (unix ms).
    pub restart_window_start_ms: u64,
    /// Audit log of lifecycle events.
    pub audit_log: VecDeque<LifecycleAuditEntry>,
}

impl ManagedConnector {
    const MAX_AUDIT_LOG_SIZE: usize = 100;

    /// Create a new managed connector from a verified install.
    fn from_install(manifest: &ConnectorManifest, trust_level: TrustLevel, now_ms: u64) -> Self {
        Self {
            connector_id: manifest.package_id.clone(),
            version: manifest.version.clone(),
            display_name: manifest.display_name.clone(),
            admin_state: AdminState::Enabled,
            runtime_phase: ConnectorLifecyclePhase::Stopped,
            trust_level,
            granted_capabilities: manifest.required_capabilities.clone(),
            installed_at_ms: now_ms,
            last_transition_at_ms: now_ms,
            previous_version: None,
            rollback_manifest: None,
            restart_count: 0,
            restart_window_start_ms: now_ms,
            audit_log: VecDeque::new(),
        }
    }

    fn push_audit(&mut self, entry: LifecycleAuditEntry) {
        if self.audit_log.len() >= Self::MAX_AUDIT_LOG_SIZE {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(entry);
    }
}

/// Auditable lifecycle event record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleAuditEntry {
    pub at_ms: u64,
    pub operation: String,
    pub old_admin_state: AdminState,
    pub new_admin_state: AdminState,
    pub old_phase: ConnectorLifecyclePhase,
    pub new_phase: ConnectorLifecyclePhase,
    pub detail: String,
}

// =============================================================================
// Restart policy
// =============================================================================

/// Controls how aggressively connectors may be restarted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartPolicy {
    /// Maximum restarts allowed within the window.
    pub max_restarts: u32,
    /// Window duration in seconds.
    pub window_secs: u64,
    /// Cooldown between restarts in milliseconds.
    pub cooldown_ms: u64,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            window_secs: 300,
            cooldown_ms: 2000,
        }
    }
}

impl RestartPolicy {
    /// Strict policy: fewer restarts, longer cooldown.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            max_restarts: 3,
            window_secs: 600,
            cooldown_ms: 5000,
        }
    }

    /// Lenient policy: more restarts, shorter cooldown.
    #[must_use]
    pub fn lenient() -> Self {
        Self {
            max_restarts: 10,
            window_secs: 120,
            cooldown_ms: 500,
        }
    }
}

// =============================================================================
// Upgrade strategy
// =============================================================================

/// Strategy for handling version upgrades.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeStrategy {
    /// Stop → upgrade → start (simple, has downtime).
    StopAndReplace,
    /// Install new version alongside, switch when healthy, remove old.
    BlueGreen,
    /// Mark as upgrading, drain in-flight, switch, verify.
    RollingDrain,
}

impl Default for UpgradeStrategy {
    fn default() -> Self {
        Self::StopAndReplace
    }
}

// =============================================================================
// Lifecycle operation result
// =============================================================================

/// Result of executing a lifecycle intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleResult {
    pub connector_id: String,
    pub operation: String,
    pub success: bool,
    pub admin_state: AdminState,
    pub runtime_phase: ConnectorLifecyclePhase,
    pub detail: String,
    pub at_ms: u64,
}

// =============================================================================
// Lifecycle manager
// =============================================================================

/// Configuration for the lifecycle manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleManagerConfig {
    pub trust_policy: TrustPolicy,
    pub restart_policy: RestartPolicy,
    pub upgrade_strategy: UpgradeStrategy,
    /// Maximum connectors that can be managed simultaneously.
    pub max_managed_connectors: usize,
}

impl Default for LifecycleManagerConfig {
    fn default() -> Self {
        Self {
            trust_policy: TrustPolicy::default(),
            restart_policy: RestartPolicy::default(),
            upgrade_strategy: UpgradeStrategy::default(),
            max_managed_connectors: 64,
        }
    }
}

/// The lifecycle manager: central orchestrator for connector install/update/enable/disable/restart.
#[derive(Debug)]
pub struct ConnectorLifecycleManager {
    config: LifecycleManagerConfig,
    connectors: BTreeMap<String, ManagedConnector>,
    /// Global operation counter for correlation.
    op_counter: u64,
}

impl ConnectorLifecycleManager {
    /// Create a new lifecycle manager with the given configuration.
    #[must_use]
    pub fn new(config: LifecycleManagerConfig) -> Self {
        Self {
            config,
            connectors: BTreeMap::new(),
            op_counter: 0,
        }
    }

    /// Number of currently managed connectors.
    #[must_use]
    pub fn count(&self) -> usize {
        self.connectors.len()
    }

    /// Get a snapshot of a managed connector.
    #[must_use]
    pub fn get(&self, connector_id: &str) -> Option<&ManagedConnector> {
        self.connectors.get(connector_id)
    }

    /// List all managed connector IDs.
    #[must_use]
    pub fn list_ids(&self) -> Vec<&str> {
        self.connectors.keys().map(|s| s.as_str()).collect()
    }

    /// List connectors in a particular admin state.
    #[must_use]
    pub fn list_by_state(&self, state: AdminState) -> Vec<&ManagedConnector> {
        self.connectors
            .values()
            .filter(|c| c.admin_state == state)
            .collect()
    }

    /// List connectors in a particular runtime phase.
    #[must_use]
    pub fn list_by_phase(&self, phase: ConnectorLifecyclePhase) -> Vec<&ManagedConnector> {
        self.connectors
            .values()
            .filter(|c| c.runtime_phase == phase)
            .collect()
    }

    // -------------------------------------------------------------------------
    // Execute intents
    // -------------------------------------------------------------------------

    /// Execute a lifecycle intent and return the result.
    pub fn execute(
        &mut self,
        intent: LifecycleIntent,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        self.op_counter += 1;
        match intent {
            LifecycleIntent::Install { manifest } => self.do_install(manifest, now_ms),
            LifecycleIntent::Update {
                connector_id,
                manifest,
            } => self.do_update(&connector_id, manifest, now_ms),
            LifecycleIntent::Enable { connector_id } => self.do_enable(&connector_id, now_ms),
            LifecycleIntent::Disable {
                connector_id,
                reason,
            } => self.do_disable(&connector_id, &reason, now_ms),
            LifecycleIntent::Restart { connector_id } => self.do_restart(&connector_id, now_ms),
            LifecycleIntent::Uninstall { connector_id } => self.do_uninstall(&connector_id, now_ms),
            LifecycleIntent::Rollback { connector_id } => self.do_rollback(&connector_id, now_ms),
        }
    }

    // -------------------------------------------------------------------------
    // Install
    // -------------------------------------------------------------------------

    fn do_install(
        &mut self,
        manifest: ConnectorManifest,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        // Check capacity.
        if self.connectors.len() >= self.config.max_managed_connectors {
            return Err(ConnectorLifecycleError::UpgradePreconditionFailed {
                connector_id: manifest.package_id.clone(),
                reason: format!(
                    "max managed connectors ({}) reached",
                    self.config.max_managed_connectors
                ),
            });
        }

        // Check not already installed.
        if let Some(existing) = self.connectors.get(&manifest.package_id) {
            return Err(ConnectorLifecycleError::AlreadyInstalled {
                connector_id: manifest.package_id.clone(),
                version: existing.version.clone(),
            });
        }

        // Validate manifest structure.
        manifest.validate()?;

        // Run trust policy gate.
        let trust_level = self.config.trust_policy.gate(&manifest)?;

        // Create managed connector.
        let mut mc = ManagedConnector::from_install(&manifest, trust_level, now_ms);
        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "install".to_string(),
            old_admin_state: AdminState::Disabled, // conceptual "before"
            new_admin_state: AdminState::Enabled,
            old_phase: ConnectorLifecyclePhase::Stopped,
            new_phase: ConnectorLifecyclePhase::Stopped,
            detail: format!("installed v{} (trust: {})", manifest.version, trust_level),
        });

        let result = LifecycleResult {
            connector_id: manifest.package_id.clone(),
            operation: "install".to_string(),
            success: true,
            admin_state: mc.admin_state,
            runtime_phase: mc.runtime_phase,
            detail: format!(
                "installed v{} with trust level {}",
                manifest.version, trust_level
            ),
            at_ms: now_ms,
        };

        self.connectors.insert(manifest.package_id.clone(), mc);
        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Update (version upgrade)
    // -------------------------------------------------------------------------

    fn do_update(
        &mut self,
        connector_id: &str,
        manifest: ConnectorManifest,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        // Can't update while uninstalling.
        if mc.admin_state == AdminState::Uninstalling {
            return Err(ConnectorLifecycleError::InvalidTransition {
                connector_id: connector_id.to_string(),
                from: mc.admin_state.to_string(),
                to: "upgrading".to_string(),
            });
        }

        if manifest.package_id != connector_id {
            return Err(ConnectorLifecycleError::UpgradePreconditionFailed {
                connector_id: connector_id.to_string(),
                reason: format!(
                    "manifest package_id {} does not match target connector {}",
                    manifest.package_id, connector_id
                ),
            });
        }

        // Validate the new manifest.
        manifest.validate()?;

        // Trust policy gate.
        let trust_level = self.config.trust_policy.gate(&manifest)?;

        // Version must differ.
        if mc.version == manifest.version {
            return Err(ConnectorLifecycleError::UpgradePreconditionFailed {
                connector_id: connector_id.to_string(),
                reason: format!("already at version {}", mc.version),
            });
        }

        let old_state = mc.admin_state;
        let old_version = mc.version.clone();
        let old_phase = mc.runtime_phase;
        let old_display_name = mc.display_name.clone();
        let old_capabilities = mc.granted_capabilities.clone();

        // Save rollback info.
        mc.previous_version = Some(old_version.clone());
        // Snapshot rollback metadata from pre-upgrade state rather than the incoming manifest.
        let mut rollback = manifest.clone();
        rollback.version = old_version.clone();
        rollback.package_id = connector_id.to_string();
        rollback.display_name = old_display_name;
        rollback.required_capabilities = old_capabilities;
        mc.rollback_manifest = Some(rollback);

        // Apply update.
        mc.version = manifest.version.clone();
        mc.display_name = manifest.display_name.clone();
        mc.trust_level = trust_level;
        mc.granted_capabilities = manifest.required_capabilities.clone();
        mc.admin_state = AdminState::Upgrading;
        mc.last_transition_at_ms = now_ms;

        // In stop-and-replace, the connector stops during upgrade.
        if self.config.upgrade_strategy == UpgradeStrategy::StopAndReplace {
            mc.runtime_phase = ConnectorLifecyclePhase::Stopped;
        }

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "update".to_string(),
            old_admin_state: old_state,
            new_admin_state: mc.admin_state,
            old_phase,
            new_phase: mc.runtime_phase,
            detail: format!(
                "updated {} -> {} (trust: {})",
                old_version, manifest.version, trust_level
            ),
        });

        // After upgrade completes, transition back to enabled.
        mc.admin_state = AdminState::Enabled;

        let result = LifecycleResult {
            connector_id: connector_id.to_string(),
            operation: "update".to_string(),
            success: true,
            admin_state: mc.admin_state,
            runtime_phase: mc.runtime_phase,
            detail: format!(
                "updated from v{} to v{} (strategy: {:?})",
                old_version, manifest.version, self.config.upgrade_strategy
            ),
            at_ms: now_ms,
        };

        Ok(result)
    }

    // -------------------------------------------------------------------------
    // Enable
    // -------------------------------------------------------------------------

    fn do_enable(
        &mut self,
        connector_id: &str,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        if mc.admin_state == AdminState::Uninstalling {
            return Err(ConnectorLifecycleError::InvalidTransition {
                connector_id: connector_id.to_string(),
                from: mc.admin_state.to_string(),
                to: "enabled".to_string(),
            });
        }

        let old_state = mc.admin_state;
        mc.admin_state = AdminState::Enabled;
        mc.last_transition_at_ms = now_ms;

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "enable".to_string(),
            old_admin_state: old_state,
            new_admin_state: AdminState::Enabled,
            old_phase: mc.runtime_phase,
            new_phase: mc.runtime_phase,
            detail: format!("enabled (was {})", old_state),
        });

        Ok(LifecycleResult {
            connector_id: connector_id.to_string(),
            operation: "enable".to_string(),
            success: true,
            admin_state: mc.admin_state,
            runtime_phase: mc.runtime_phase,
            detail: format!("transitioned from {} to enabled", old_state),
            at_ms: now_ms,
        })
    }

    // -------------------------------------------------------------------------
    // Disable
    // -------------------------------------------------------------------------

    fn do_disable(
        &mut self,
        connector_id: &str,
        reason: &str,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        if mc.admin_state == AdminState::Uninstalling {
            return Err(ConnectorLifecycleError::InvalidTransition {
                connector_id: connector_id.to_string(),
                from: mc.admin_state.to_string(),
                to: "disabled".to_string(),
            });
        }

        let old_state = mc.admin_state;
        let old_phase = mc.runtime_phase;
        mc.admin_state = AdminState::Disabled;
        mc.runtime_phase = ConnectorLifecyclePhase::Stopped;
        mc.last_transition_at_ms = now_ms;

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "disable".to_string(),
            old_admin_state: old_state,
            new_admin_state: AdminState::Disabled,
            old_phase,
            new_phase: ConnectorLifecyclePhase::Stopped,
            detail: format!("disabled: {reason}"),
        });

        Ok(LifecycleResult {
            connector_id: connector_id.to_string(),
            operation: "disable".to_string(),
            success: true,
            admin_state: mc.admin_state,
            runtime_phase: mc.runtime_phase,
            detail: format!("disabled (was {}): {}", old_state, reason),
            at_ms: now_ms,
        })
    }

    // -------------------------------------------------------------------------
    // Restart
    // -------------------------------------------------------------------------

    fn do_restart(
        &mut self,
        connector_id: &str,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        if !mc.admin_state.can_start() {
            return Err(ConnectorLifecycleError::InvalidTransition {
                connector_id: connector_id.to_string(),
                from: mc.admin_state.to_string(),
                to: "restart".to_string(),
            });
        }

        // Check restart rate limit.
        let window_ms = self.config.restart_policy.window_secs.saturating_mul(1000);
        if now_ms.saturating_sub(mc.restart_window_start_ms) >= window_ms {
            // Reset window.
            mc.restart_count = 0;
            mc.restart_window_start_ms = now_ms;
        }

        if let Some(last_restart_at_ms) = mc
            .audit_log
            .iter()
            .rev()
            .find(|entry| entry.operation == "restart")
            .map(|entry| entry.at_ms)
        {
            let since_last_restart = now_ms.saturating_sub(last_restart_at_ms);
            if since_last_restart < self.config.restart_policy.cooldown_ms {
                return Err(ConnectorLifecycleError::InvalidTransition {
                    connector_id: connector_id.to_string(),
                    from: mc.admin_state.to_string(),
                    to: "restart_cooldown".to_string(),
                });
            }
        }

        mc.restart_count += 1;
        if mc.restart_count > self.config.restart_policy.max_restarts {
            return Err(ConnectorLifecycleError::RestartLimitExceeded {
                connector_id: connector_id.to_string(),
                count: mc.restart_count,
                window_secs: self.config.restart_policy.window_secs,
            });
        }

        let old_phase = mc.runtime_phase;
        // Restart: stop then start.
        mc.runtime_phase = ConnectorLifecyclePhase::Starting;
        mc.last_transition_at_ms = now_ms;

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "restart".to_string(),
            old_admin_state: mc.admin_state,
            new_admin_state: mc.admin_state,
            old_phase,
            new_phase: ConnectorLifecyclePhase::Starting,
            detail: format!("restart #{} in window", mc.restart_count),
        });

        Ok(LifecycleResult {
            connector_id: connector_id.to_string(),
            operation: "restart".to_string(),
            success: true,
            admin_state: mc.admin_state,
            runtime_phase: mc.runtime_phase,
            detail: format!(
                "restarting (attempt {} of {} in {}s window)",
                mc.restart_count,
                self.config.restart_policy.max_restarts,
                self.config.restart_policy.window_secs
            ),
            at_ms: now_ms,
        })
    }

    // -------------------------------------------------------------------------
    // Uninstall
    // -------------------------------------------------------------------------

    fn do_uninstall(
        &mut self,
        connector_id: &str,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        let old_state = mc.admin_state;
        let old_phase = mc.runtime_phase;
        mc.admin_state = AdminState::Uninstalling;
        mc.runtime_phase = ConnectorLifecyclePhase::Stopped;
        mc.last_transition_at_ms = now_ms;

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "uninstall".to_string(),
            old_admin_state: old_state,
            new_admin_state: AdminState::Uninstalling,
            old_phase,
            new_phase: ConnectorLifecyclePhase::Stopped,
            detail: format!("uninstalling v{}", mc.version),
        });

        // Remove from registry.
        let version = mc.version.clone();
        self.connectors.remove(connector_id);

        Ok(LifecycleResult {
            connector_id: connector_id.to_string(),
            operation: "uninstall".to_string(),
            success: true,
            admin_state: AdminState::Uninstalling,
            runtime_phase: ConnectorLifecyclePhase::Stopped,
            detail: format!("uninstalled v{}", version),
            at_ms: now_ms,
        })
    }

    // -------------------------------------------------------------------------
    // Rollback
    // -------------------------------------------------------------------------

    fn do_rollback(
        &mut self,
        connector_id: &str,
        now_ms: u64,
    ) -> Result<LifecycleResult, ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        let previous_version =
            mc.previous_version
                .clone()
                .ok_or_else(|| ConnectorLifecycleError::RollbackFailed {
                    connector_id: connector_id.to_string(),
                    reason: "no previous version to rollback to".to_string(),
                })?;

        let rollback_manifest = mc.rollback_manifest.clone().ok_or_else(|| {
            ConnectorLifecycleError::RollbackFailed {
                connector_id: connector_id.to_string(),
                reason: "rollback manifest not available".to_string(),
            }
        })?;

        let current_version = mc.version.clone();
        let old_state = mc.admin_state;
        let old_phase = mc.runtime_phase;

        // Apply rollback.
        mc.version = previous_version.clone();
        mc.display_name = rollback_manifest.display_name.clone();
        mc.granted_capabilities = rollback_manifest.required_capabilities.clone();
        mc.previous_version = None;
        mc.rollback_manifest = None;
        mc.admin_state = AdminState::Enabled;
        mc.runtime_phase = ConnectorLifecyclePhase::Stopped;
        mc.last_transition_at_ms = now_ms;

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "rollback".to_string(),
            old_admin_state: old_state,
            new_admin_state: AdminState::Enabled,
            old_phase,
            new_phase: ConnectorLifecyclePhase::Stopped,
            detail: format!(
                "rolled back from v{} to v{}",
                current_version, previous_version
            ),
        });

        Ok(LifecycleResult {
            connector_id: connector_id.to_string(),
            operation: "rollback".to_string(),
            success: true,
            admin_state: mc.admin_state,
            runtime_phase: mc.runtime_phase,
            detail: format!(
                "rolled back from v{} to v{}",
                current_version, previous_version
            ),
            at_ms: now_ms,
        })
    }

    // -------------------------------------------------------------------------
    // Runtime phase updates (from host runtime callbacks)
    // -------------------------------------------------------------------------

    /// Notify the lifecycle manager that a connector's runtime phase changed.
    /// Called by the host runtime when startup completes, health checks fail, etc.
    pub fn notify_phase_change(
        &mut self,
        connector_id: &str,
        new_phase: ConnectorLifecyclePhase,
        now_ms: u64,
    ) -> Result<(), ConnectorLifecycleError> {
        let mc = self.connectors.get_mut(connector_id).ok_or_else(|| {
            ConnectorLifecycleError::NotInstalled {
                connector_id: connector_id.to_string(),
            }
        })?;

        let old_phase = mc.runtime_phase;
        mc.runtime_phase = new_phase;
        mc.last_transition_at_ms = now_ms;

        mc.push_audit(LifecycleAuditEntry {
            at_ms: now_ms,
            operation: "phase_change".to_string(),
            old_admin_state: mc.admin_state,
            new_admin_state: mc.admin_state,
            old_phase,
            new_phase,
            detail: format!("runtime: {} -> {}", old_phase, new_phase),
        });

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Query helpers
    // -------------------------------------------------------------------------

    /// Get the audit log for a connector.
    #[must_use]
    pub fn audit_log(&self, connector_id: &str) -> Option<&VecDeque<LifecycleAuditEntry>> {
        self.connectors.get(connector_id).map(|mc| &mc.audit_log)
    }

    /// Check if a connector is eligible for restart (enabled + within rate limit).
    #[must_use]
    pub fn can_restart(&self, connector_id: &str, now_ms: u64) -> bool {
        let Some(mc) = self.connectors.get(connector_id) else {
            return false;
        };
        if !mc.admin_state.can_start() {
            return false;
        }

        if let Some(last_restart_at_ms) = mc
            .audit_log
            .iter()
            .rev()
            .find(|entry| entry.operation == "restart")
            .map(|entry| entry.at_ms)
        {
            let since_last_restart = now_ms.saturating_sub(last_restart_at_ms);
            if since_last_restart < self.config.restart_policy.cooldown_ms {
                return false;
            }
        }

        let window_ms = self.config.restart_policy.window_secs.saturating_mul(1000);
        let effective_count = if now_ms.saturating_sub(mc.restart_window_start_ms) >= window_ms {
            0
        } else {
            mc.restart_count
        };
        effective_count < self.config.restart_policy.max_restarts
    }

    /// Check if a connector has a rollback target.
    #[must_use]
    pub fn can_rollback(&self, connector_id: &str) -> bool {
        self.connectors
            .get(connector_id)
            .is_some_and(|mc| mc.previous_version.is_some() && mc.rollback_manifest.is_some())
    }

    /// Current operation counter (for correlation/debugging).
    #[must_use]
    pub fn op_counter(&self) -> u64 {
        self.op_counter
    }

    /// Summary of all managed connectors.
    #[must_use]
    pub fn summary(&self) -> LifecycleManagerSummary {
        let mut enabled = 0u32;
        let mut disabled = 0u32;
        let mut running = 0u32;
        let mut stopped = 0u32;
        let mut degraded = 0u32;
        let mut failed = 0u32;

        for mc in self.connectors.values() {
            match mc.admin_state {
                AdminState::Enabled => enabled += 1,
                AdminState::Disabled => disabled += 1,
                AdminState::Upgrading => {}
                AdminState::Uninstalling => {}
            }
            match mc.runtime_phase {
                ConnectorLifecyclePhase::Running => running += 1,
                ConnectorLifecyclePhase::Stopped => stopped += 1,
                ConnectorLifecyclePhase::Degraded => degraded += 1,
                ConnectorLifecyclePhase::Failed => failed += 1,
                ConnectorLifecyclePhase::Starting => {}
            }
        }

        LifecycleManagerSummary {
            total: self.connectors.len() as u32,
            enabled,
            disabled,
            running,
            stopped,
            degraded,
            failed,
        }
    }
}

/// Summary statistics for the lifecycle manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleManagerSummary {
    pub total: u32,
    pub enabled: u32,
    pub disabled: u32,
    pub running: u32,
    pub stopped: u32,
    pub degraded: u32,
    pub failed: u32,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_manifest(id: &str, version: &str) -> ConnectorManifest {
        ConnectorManifest {
            schema_version: 1,
            package_id: id.to_string(),
            version: version.to_string(),
            display_name: format!("Test {id}"),
            description: "test connector".to_string(),
            author: "test-publisher".to_string(),
            min_ft_version: None,
            sha256_digest: "a".repeat(64),
            required_capabilities: vec![ConnectorCapability::Invoke],
            publisher_signature: Some("sig".to_string()),
            transparency_token: None,
            created_at_ms: 1000,
            metadata: BTreeMap::new(),
        }
    }

    fn test_manager() -> ConnectorLifecycleManager {
        let mut config = LifecycleManagerConfig::default();
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        ConnectorLifecycleManager::new(config)
    }

    // -- Install tests --

    #[test]
    fn install_creates_enabled_stopped_connector() {
        let mut mgr = test_manager();
        let manifest = test_manifest("my-conn", "1.0.0");
        let result = mgr
            .execute(LifecycleIntent::Install { manifest }, 1000)
            .unwrap();
        assert!(result.success);
        assert_eq!(result.admin_state, AdminState::Enabled);
        assert_eq!(result.runtime_phase, ConnectorLifecyclePhase::Stopped);
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn install_rejects_duplicate() {
        let mut mgr = test_manager();
        let manifest = test_manifest("my-conn", "1.0.0");
        mgr.execute(
            LifecycleIntent::Install {
                manifest: manifest.clone(),
            },
            1000,
        )
        .unwrap();
        let err = mgr
            .execute(LifecycleIntent::Install { manifest }, 2000)
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::AlreadyInstalled { .. }
        ));
    }

    #[test]
    fn install_respects_capacity_limit() {
        let mut config = LifecycleManagerConfig::default();
        config.max_managed_connectors = 2;
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        let mut mgr = ConnectorLifecycleManager::new(config);

        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("a", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("b", "1.0.0"),
            },
            2000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Install {
                    manifest: test_manifest("c", "1.0.0"),
                },
                3000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::UpgradePreconditionFailed { .. }
        ));
    }

    #[test]
    fn install_evaluates_trust_policy() {
        let mut mgr = test_manager();
        let manifest = test_manifest("my-conn", "1.0.0");
        mgr.execute(LifecycleIntent::Install { manifest }, 1000)
            .unwrap();
        let mc = mgr.get("my-conn").unwrap();
        assert_eq!(mc.trust_level, TrustLevel::Trusted);
    }

    #[test]
    fn install_rejects_untrusted() {
        let mut mgr = test_manager();
        let mut manifest = test_manifest("bad-conn", "1.0.0");
        manifest.publisher_signature = None; // no signature → untrusted
        let err = mgr
            .execute(LifecycleIntent::Install { manifest }, 1000)
            .unwrap_err();
        assert!(matches!(err, ConnectorLifecycleError::Registry(_)));
    }

    // -- Enable/Disable tests --

    #[test]
    fn disable_stops_runtime() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.notify_phase_change("c", ConnectorLifecyclePhase::Running, 1500)
            .unwrap();

        let result = mgr
            .execute(
                LifecycleIntent::Disable {
                    connector_id: "c".to_string(),
                    reason: "maintenance".to_string(),
                },
                2000,
            )
            .unwrap();
        assert_eq!(result.admin_state, AdminState::Disabled);
        assert_eq!(result.runtime_phase, ConnectorLifecyclePhase::Stopped);
    }

    #[test]
    fn enable_after_disable() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Disable {
                connector_id: "c".to_string(),
                reason: "test".to_string(),
            },
            2000,
        )
        .unwrap();
        let result = mgr
            .execute(
                LifecycleIntent::Enable {
                    connector_id: "c".to_string(),
                },
                3000,
            )
            .unwrap();
        assert_eq!(result.admin_state, AdminState::Enabled);
    }

    #[test]
    fn cannot_enable_uninstalling() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        // Manually set to uninstalling state.
        mgr.connectors.get_mut("c").unwrap().admin_state = AdminState::Uninstalling;
        let err = mgr
            .execute(
                LifecycleIntent::Enable {
                    connector_id: "c".to_string(),
                },
                2000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::InvalidTransition { .. }
        ));
    }

    // -- Restart tests --

    #[test]
    fn restart_transitions_to_starting() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        let result = mgr
            .execute(
                LifecycleIntent::Restart {
                    connector_id: "c".to_string(),
                },
                2000,
            )
            .unwrap();
        assert_eq!(result.runtime_phase, ConnectorLifecyclePhase::Starting);
    }

    #[test]
    fn restart_rate_limited() {
        let mut config = LifecycleManagerConfig::default();
        config.restart_policy.max_restarts = 2;
        config.restart_policy.window_secs = 60;
        config.restart_policy.cooldown_ms = 0;
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        let mut mgr = ConnectorLifecycleManager::new(config);

        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            3000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Restart {
                    connector_id: "c".to_string(),
                },
                4000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::RestartLimitExceeded { .. }
        ));
    }

    #[test]
    fn restart_window_resets() {
        let mut config = LifecycleManagerConfig::default();
        config.restart_policy.max_restarts = 2;
        config.restart_policy.window_secs = 10;
        config.restart_policy.cooldown_ms = 0;
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        let mut mgr = ConnectorLifecycleManager::new(config);

        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            3000,
        )
        .unwrap();
        // Should fail within window.
        assert!(
            mgr.execute(
                LifecycleIntent::Restart {
                    connector_id: "c".to_string()
                },
                4000
            )
            .is_err()
        );
        // Window resets after 10s.
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            15000,
        )
        .unwrap();
    }

    #[test]
    fn restart_window_resets_at_exact_boundary() {
        let mut config = LifecycleManagerConfig::default();
        config.restart_policy.max_restarts = 1;
        config.restart_policy.window_secs = 10;
        config.restart_policy.cooldown_ms = 0;
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        let mut mgr = ConnectorLifecycleManager::new(config);

        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();

        // Exactly at boundary: 11000 - 1000 = 10000ms.
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            11000,
        )
        .unwrap();
    }

    #[test]
    fn restart_cooldown_enforced() {
        let mut config = LifecycleManagerConfig::default();
        config.restart_policy.max_restarts = 10;
        config.restart_policy.window_secs = 60;
        config.restart_policy.cooldown_ms = 2000;
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        let mut mgr = ConnectorLifecycleManager::new(config);

        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();

        let err = mgr
            .execute(
                LifecycleIntent::Restart {
                    connector_id: "c".to_string(),
                },
                2500,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::InvalidTransition { .. }
        ));
    }

    #[test]
    fn can_restart_respects_cooldown() {
        let mut config = LifecycleManagerConfig::default();
        config.restart_policy.max_restarts = 10;
        config.restart_policy.window_secs = 60;
        config.restart_policy.cooldown_ms = 2000;
        config
            .trust_policy
            .trusted_publishers
            .push("test-publisher".to_string());
        let mut mgr = ConnectorLifecycleManager::new(config);

        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();

        assert!(!mgr.can_restart("c", 2500));
        assert!(mgr.can_restart("c", 4000));
    }

    #[test]
    fn cannot_restart_disabled() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Disable {
                connector_id: "c".to_string(),
                reason: "test".to_string(),
            },
            2000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Restart {
                    connector_id: "c".to_string(),
                },
                3000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::InvalidTransition { .. }
        ));
    }

    // -- Update tests --

    #[test]
    fn update_changes_version() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        let result = mgr
            .execute(
                LifecycleIntent::Update {
                    connector_id: "c".to_string(),
                    manifest: test_manifest("c", "2.0.0"),
                },
                2000,
            )
            .unwrap();
        assert!(result.success);
        assert_eq!(mgr.get("c").unwrap().version, "2.0.0");
    }

    #[test]
    fn update_preserves_rollback_info() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Update {
                connector_id: "c".to_string(),
                manifest: test_manifest("c", "2.0.0"),
            },
            2000,
        )
        .unwrap();
        let mc = mgr.get("c").unwrap();
        assert_eq!(mc.previous_version.as_deref(), Some("1.0.0"));
        assert!(mc.rollback_manifest.is_some());
    }

    #[test]
    fn update_rejects_same_version() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Update {
                    connector_id: "c".to_string(),
                    manifest: test_manifest("c", "1.0.0"),
                },
                2000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::UpgradePreconditionFailed { .. }
        ));
    }

    #[test]
    fn update_rejects_manifest_connector_id_mismatch() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Update {
                    connector_id: "c".to_string(),
                    manifest: test_manifest("other-connector", "2.0.0"),
                },
                2000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::UpgradePreconditionFailed { .. }
        ));
    }

    // -- Rollback tests --

    #[test]
    fn rollback_restores_previous_version() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Update {
                connector_id: "c".to_string(),
                manifest: test_manifest("c", "2.0.0"),
            },
            2000,
        )
        .unwrap();
        let result = mgr
            .execute(
                LifecycleIntent::Rollback {
                    connector_id: "c".to_string(),
                },
                3000,
            )
            .unwrap();
        assert!(result.success);
        assert_eq!(mgr.get("c").unwrap().version, "1.0.0");
    }

    #[test]
    fn rollback_restores_previous_display_name_and_capabilities() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();

        let mut upgraded = test_manifest("c", "2.0.0");
        upgraded.display_name = "Updated Connector C".to_string();
        upgraded.required_capabilities = vec![ConnectorCapability::ReadState];

        mgr.execute(
            LifecycleIntent::Update {
                connector_id: "c".to_string(),
                manifest: upgraded,
            },
            2000,
        )
        .unwrap();

        mgr.execute(
            LifecycleIntent::Rollback {
                connector_id: "c".to_string(),
            },
            3000,
        )
        .unwrap();
        let mc = mgr.get("c").unwrap();
        assert_eq!(mc.version, "1.0.0");
        assert_eq!(mc.display_name, "Test c");
        assert_eq!(mc.granted_capabilities, vec![ConnectorCapability::Invoke]);
    }

    #[test]
    fn rollback_clears_rollback_info() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Update {
                connector_id: "c".to_string(),
                manifest: test_manifest("c", "2.0.0"),
            },
            2000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Rollback {
                connector_id: "c".to_string(),
            },
            3000,
        )
        .unwrap();
        let mc = mgr.get("c").unwrap();
        assert!(mc.previous_version.is_none());
        assert!(mc.rollback_manifest.is_none());
    }

    #[test]
    fn rollback_fails_without_previous_version() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Rollback {
                    connector_id: "c".to_string(),
                },
                2000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::RollbackFailed { .. }
        ));
    }

    #[test]
    fn double_rollback_fails() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Update {
                connector_id: "c".to_string(),
                manifest: test_manifest("c", "2.0.0"),
            },
            2000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Rollback {
                connector_id: "c".to_string(),
            },
            3000,
        )
        .unwrap();
        let err = mgr
            .execute(
                LifecycleIntent::Rollback {
                    connector_id: "c".to_string(),
                },
                4000,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ConnectorLifecycleError::RollbackFailed { .. }
        ));
    }

    // -- Uninstall tests --

    #[test]
    fn uninstall_removes_connector() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        assert_eq!(mgr.count(), 1);
        mgr.execute(
            LifecycleIntent::Uninstall {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();
        assert_eq!(mgr.count(), 0);
        assert!(mgr.get("c").is_none());
    }

    #[test]
    fn uninstall_not_found() {
        let mut mgr = test_manager();
        let err = mgr
            .execute(
                LifecycleIntent::Uninstall {
                    connector_id: "x".to_string(),
                },
                1000,
            )
            .unwrap_err();
        assert!(matches!(err, ConnectorLifecycleError::NotInstalled { .. }));
    }

    // -- Phase notification tests --

    #[test]
    fn phase_change_updates_runtime() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.notify_phase_change("c", ConnectorLifecyclePhase::Running, 2000)
            .unwrap();
        assert_eq!(
            mgr.get("c").unwrap().runtime_phase,
            ConnectorLifecyclePhase::Running
        );
    }

    #[test]
    fn phase_change_unknown_connector() {
        let mut mgr = test_manager();
        let err = mgr
            .notify_phase_change("x", ConnectorLifecyclePhase::Running, 1000)
            .unwrap_err();
        assert!(matches!(err, ConnectorLifecycleError::NotInstalled { .. }));
    }

    // -- Query helper tests --

    #[test]
    fn list_by_state_and_phase() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("a", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("b", "1.0.0"),
            },
            2000,
        )
        .unwrap();
        mgr.notify_phase_change("a", ConnectorLifecyclePhase::Running, 3000)
            .unwrap();
        mgr.execute(
            LifecycleIntent::Disable {
                connector_id: "b".to_string(),
                reason: "test".to_string(),
            },
            4000,
        )
        .unwrap();

        assert_eq!(mgr.list_by_state(AdminState::Enabled).len(), 1);
        assert_eq!(mgr.list_by_state(AdminState::Disabled).len(), 1);
        assert_eq!(mgr.list_by_phase(ConnectorLifecyclePhase::Running).len(), 1);
    }

    #[test]
    fn can_restart_checks() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        assert!(mgr.can_restart("c", 2000));
        assert!(!mgr.can_restart("nonexistent", 2000));
    }

    #[test]
    fn can_rollback_checks() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        assert!(!mgr.can_rollback("c"));
        mgr.execute(
            LifecycleIntent::Update {
                connector_id: "c".to_string(),
                manifest: test_manifest("c", "2.0.0"),
            },
            2000,
        )
        .unwrap();
        assert!(mgr.can_rollback("c"));
    }

    // -- Audit log tests --

    #[test]
    fn audit_log_records_operations() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();
        let log = mgr.audit_log("c").unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].operation, "install");
        assert_eq!(log[1].operation, "restart");
    }

    #[test]
    fn audit_log_bounded() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        // Generate 150 phase changes (exceeding 100 limit).
        for i in 0..150 {
            mgr.notify_phase_change("c", ConnectorLifecyclePhase::Running, 2000 + i)
                .unwrap();
        }
        let log = mgr.audit_log("c").unwrap();
        assert!(log.len() <= ManagedConnector::MAX_AUDIT_LOG_SIZE);
    }

    // -- Summary tests --

    #[test]
    fn summary_counts() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("a", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("b", "1.0.0"),
            },
            2000,
        )
        .unwrap();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            3000,
        )
        .unwrap();
        mgr.notify_phase_change("a", ConnectorLifecyclePhase::Running, 4000)
            .unwrap();
        mgr.notify_phase_change("b", ConnectorLifecyclePhase::Failed, 4500)
            .unwrap();
        mgr.execute(
            LifecycleIntent::Disable {
                connector_id: "c".to_string(),
                reason: "test".to_string(),
            },
            5000,
        )
        .unwrap();

        let summary = mgr.summary();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.enabled, 2);
        assert_eq!(summary.disabled, 1);
        assert_eq!(summary.running, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.stopped, 1); // c is disabled+stopped
    }

    // -- LifecycleIntent tests --

    #[test]
    fn intent_connector_id() {
        let intent = LifecycleIntent::Install {
            manifest: test_manifest("x", "1.0.0"),
        };
        assert_eq!(intent.connector_id(), "x");
        assert_eq!(intent.op_name(), "install");

        let intent = LifecycleIntent::Restart {
            connector_id: "y".to_string(),
        };
        assert_eq!(intent.connector_id(), "y");
        assert_eq!(intent.op_name(), "restart");
    }

    // -- Op counter tests --

    #[test]
    fn op_counter_increments() {
        let mut mgr = test_manager();
        assert_eq!(mgr.op_counter(), 0);
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        assert_eq!(mgr.op_counter(), 1);
        mgr.execute(
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            2000,
        )
        .unwrap();
        assert_eq!(mgr.op_counter(), 2);
    }

    // -- Serde roundtrip tests --

    #[test]
    fn admin_state_serde_roundtrip() {
        for state in [
            AdminState::Enabled,
            AdminState::Disabled,
            AdminState::Upgrading,
            AdminState::Uninstalling,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: AdminState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn lifecycle_result_serde_roundtrip() {
        let result = LifecycleResult {
            connector_id: "test".to_string(),
            operation: "install".to_string(),
            success: true,
            admin_state: AdminState::Enabled,
            runtime_phase: ConnectorLifecyclePhase::Running,
            detail: "ok".to_string(),
            at_ms: 12345,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: LifecycleResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
    }

    #[test]
    fn managed_connector_serde_roundtrip() {
        let mut mgr = test_manager();
        mgr.execute(
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            1000,
        )
        .unwrap();
        let mc = mgr.get("c").unwrap();
        let json = serde_json::to_string(mc).unwrap();
        let back: ManagedConnector = serde_json::from_str(&json).unwrap();
        assert_eq!(mc.connector_id, back.connector_id);
        assert_eq!(mc.version, back.version);
        assert_eq!(mc.admin_state, back.admin_state);
    }

    #[test]
    fn lifecycle_intent_serde_roundtrip() {
        let intents = vec![
            LifecycleIntent::Install {
                manifest: test_manifest("c", "1.0.0"),
            },
            LifecycleIntent::Enable {
                connector_id: "c".to_string(),
            },
            LifecycleIntent::Disable {
                connector_id: "c".to_string(),
                reason: "test".to_string(),
            },
            LifecycleIntent::Restart {
                connector_id: "c".to_string(),
            },
            LifecycleIntent::Uninstall {
                connector_id: "c".to_string(),
            },
            LifecycleIntent::Rollback {
                connector_id: "c".to_string(),
            },
        ];
        for intent in intents {
            let json = serde_json::to_string(&intent).unwrap();
            let back: LifecycleIntent = serde_json::from_str(&json).unwrap();
            assert_eq!(intent, back);
        }
    }
}
