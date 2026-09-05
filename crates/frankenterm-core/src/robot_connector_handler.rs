//! Handler for the `robot connector` family (ft-pohny).
//!
//! Wires [`PolicyEngine::run_connector_lifecycle_intent`] — the single gated
//! production boundary for connector lifecycle administration (graduated
//! kill-switch fail-closed, `op_counter` telemetry) — into a typed operator
//! surface. Contract: `docs/robot-contracts/connector.md`.
//!
//! The handler itself is storage-free: the CLI dispatch loads the persisted
//! managed-connector snapshot from the workspace DB's `config` KV table
//! (key [`CONNECTOR_LIFECYCLE_STATE_KEY`]), rehydrates the engine's manager
//! via [`ConnectorLifecycleManager::restore_connectors`], calls
//! [`handle_connector_action`], and persists the snapshot back after a
//! successful non-dry-run mutation. The state codec lives here
//! ([`decode_connector_state`] / [`encode_connector_state`]) so its
//! fail-closed semantics are unit-tested next to the handler.
//!
//! Scope split (see the contract doc): non-dry-run `uninstall` / `rollback`
//! are approval-blocked in this slice (typed
//! `robot.connector.require_approval`), matching the shipped
//! `ft robot checkpoint rollback` precedent; redemption wiring is
//! `ft-pohny.cont.approval`.
//!
//! [`ConnectorLifecycleManager::restore_connectors`]: crate::connector_lifecycle::ConnectorLifecycleManager::restore_connectors

use serde::{Deserialize, Serialize};

use crate::connector_lifecycle::{LifecycleIntent, ManagedConnector};
use crate::connector_registry::ConnectorManifest;
use crate::policy::PolicyEngine;

/// `config`-KV key holding the persisted managed-connector snapshot.
pub const CONNECTOR_LIFECYCLE_STATE_KEY: &str = "connector_lifecycle_state_v1";

// =============================================================================
// Requests
// =============================================================================

/// Typed request for the connector family.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConnectorAction {
    /// Read-only status of managed connectors (all, or one by id).
    Status { connector_id: Option<String> },
    /// Install a connector from a validated manifest.
    Install {
        manifest: Box<ConnectorManifest>,
        dry_run: bool,
    },
    /// Upgrade an installed connector to a new manifest.
    Update {
        connector_id: String,
        manifest: Box<ConnectorManifest>,
        dry_run: bool,
    },
    /// Administratively enable an installed connector.
    Enable { connector_id: String, dry_run: bool },
    /// Administratively disable an installed connector.
    Disable {
        connector_id: String,
        reason: String,
        dry_run: bool,
    },
    /// Restart an installed connector (restart-limit windows apply).
    Restart { connector_id: String, dry_run: bool },
    /// Uninstall (destructive — approval-blocked in this slice).
    Uninstall { connector_id: String, dry_run: bool },
    /// Roll back to the previous manifest (destructive — approval-blocked
    /// in this slice).
    Rollback { connector_id: String, dry_run: bool },
}

impl ConnectorAction {
    /// Operation name for envelopes/receipts.
    #[must_use]
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Status { .. } => "status",
            Self::Install { .. } => "install",
            Self::Update { .. } => "update",
            Self::Enable { .. } => "enable",
            Self::Disable { .. } => "disable",
            Self::Restart { .. } => "restart",
            Self::Uninstall { .. } => "uninstall",
            Self::Rollback { .. } => "rollback",
        }
    }

    fn dry_run(&self) -> bool {
        match self {
            Self::Status { .. } => false,
            Self::Install { dry_run, .. }
            | Self::Update { dry_run, .. }
            | Self::Enable { dry_run, .. }
            | Self::Disable { dry_run, .. }
            | Self::Restart { dry_run, .. }
            | Self::Uninstall { dry_run, .. }
            | Self::Rollback { dry_run, .. } => *dry_run,
        }
    }

    fn is_destructive(&self) -> bool {
        matches!(self, Self::Uninstall { .. } | Self::Rollback { .. })
    }

    /// The connector id the action targets (install targets the manifest's
    /// package id).
    #[must_use]
    pub fn target_connector_id(&self) -> Option<&str> {
        match self {
            Self::Status { connector_id } => connector_id.as_deref(),
            Self::Install { manifest, .. } => Some(manifest.package_id.as_str()),
            Self::Update { connector_id, .. }
            | Self::Enable { connector_id, .. }
            | Self::Disable { connector_id, .. }
            | Self::Restart { connector_id, .. }
            | Self::Uninstall { connector_id, .. }
            | Self::Rollback { connector_id, .. } => Some(connector_id.as_str()),
        }
    }
}

// =============================================================================
// Responses
// =============================================================================

/// One managed connector in a `status` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorStatusEntry {
    pub connector_id: String,
    pub version: String,
    pub display_name: String,
    pub admin_state: String,
    pub runtime_phase: String,
    pub trust_level: String,
    pub installed_at_ms: u64,
    pub last_transition_at_ms: u64,
}

impl ConnectorStatusEntry {
    fn from_managed(mc: &ManagedConnector) -> Self {
        Self {
            connector_id: mc.connector_id.clone(),
            version: mc.version.clone(),
            display_name: mc.display_name.clone(),
            admin_state: mc.admin_state.as_str().to_string(),
            runtime_phase: mc.runtime_phase.as_str().to_string(),
            trust_level: mc.trust_level.to_string(),
            installed_at_ms: mc.installed_at_ms,
            last_transition_at_ms: mc.last_transition_at_ms,
        }
    }
}

/// `status` response data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorStatusData {
    pub connectors: Vec<ConnectorStatusEntry>,
    pub kill_switch_emergency: bool,
    pub op_counter: u64,
    /// Whether a persisted state blob was present when the CLI loaded.
    pub state_persisted: bool,
}

/// Non-dry-run mutation response data (mirrors `LifecycleResult`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorMutationData {
    pub connector_id: String,
    pub operation: String,
    pub dry_run: bool,
    pub success: bool,
    pub admin_state: String,
    pub runtime_phase: String,
    pub detail: String,
    pub at_ms: u64,
    /// Set to `true` by the CLI dispatch after the state snapshot is
    /// durably written back. `false` means the in-process mutation was NOT
    /// persisted (see the contract's `state_save_failed` retry semantics).
    pub persisted: bool,
}

/// Side-effect-free dry-run plan receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorDryRunData {
    pub connector_id: String,
    pub operation: String,
    pub dry_run: bool,
    pub currently_installed: bool,
    pub current_admin_state: Option<String>,
    pub would_require_approval: bool,
    pub kill_switch_emergency: bool,
}

/// Successful outcomes of [`handle_connector_action`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConnectorActionOutcome {
    Status(ConnectorStatusData),
    DryRun(ConnectorDryRunData),
    Mutated(ConnectorMutationData),
}

// =============================================================================
// Errors
// =============================================================================

/// Typed errors for the connector family (`robot.connector.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobotConnectorError {
    /// Emergency kill switch active — all lifecycle mutations fail closed.
    KillSwitchActive,
    /// Target connector id is not in managed state.
    NotFound { connector_id: String },
    /// `install` on an id that is already installed.
    AlreadyInstalled {
        connector_id: String,
        version: String,
    },
    /// The lifecycle manager rejected the transition.
    LifecycleFailed { detail: String },
    /// Manifest failed structural validation.
    ManifestInvalid { detail: String },
    /// Non-dry-run destructive intent in the approval-blocked slice.
    RequireApproval {
        operation: String,
        connector_id: String,
    },
    /// Persisted state blob present but unparseable — fail closed.
    StateLoadFailed { detail: String },
    /// Post-mutation state persistence failed.
    StateSaveFailed { detail: String },
}

impl RobotConnectorError {
    /// Stable machine error code (`robot.connector.*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::KillSwitchActive => "robot.connector.kill_switch_active",
            Self::NotFound { .. } => "robot.connector.not_found",
            Self::AlreadyInstalled { .. } => "robot.connector.already_installed",
            Self::LifecycleFailed { .. } => "robot.connector.lifecycle_failed",
            Self::ManifestInvalid { .. } => "robot.connector.manifest_invalid",
            Self::RequireApproval { .. } => "robot.connector.require_approval",
            Self::StateLoadFailed { .. } => "robot.connector.state_load_failed",
            Self::StateSaveFailed { .. } => "robot.connector.state_save_failed",
        }
    }

    /// Human-readable message.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::KillSwitchActive => {
                "connector lifecycle denied: kill switch active".to_string()
            }
            Self::NotFound { connector_id } => {
                format!("connector '{connector_id}' is not in managed state")
            }
            Self::AlreadyInstalled {
                connector_id,
                version,
            } => format!("connector '{connector_id}' is already installed (v{version})"),
            Self::LifecycleFailed { detail } => format!("lifecycle transition failed: {detail}"),
            Self::ManifestInvalid { detail } => format!("connector manifest invalid: {detail}"),
            Self::RequireApproval {
                operation,
                connector_id,
            } => format!(
                "destructive intent '{operation}' on '{connector_id}' requires approval; \
                 this slice ships it approval-blocked (dry-run available)"
            ),
            Self::StateLoadFailed { detail } => format!(
                "persisted connector lifecycle state is unparseable (fail closed): {detail}"
            ),
            Self::StateSaveFailed { detail } => {
                format!("connector lifecycle state persistence failed: {detail}")
            }
        }
    }

    /// Operator hint, when one exists.
    #[must_use]
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Self::KillSwitchActive => {
                Some("An authorized operator must reset the kill switch before retrying lifecycle mutations.")
            }
            Self::NotFound { .. } => {
                Some("Run 'ft robot connector status' to list managed connectors.")
            }
            Self::RequireApproval { .. } => Some(
                "Preview with --dry-run. Approval-token redemption for destructive \
                 connector intents is tracked under ft-pohny.cont.approval.",
            ),
            Self::StateLoadFailed { .. } => Some(
                "Inspect the 'connector_lifecycle_state_v1' row in the workspace DB's \
                 config table; restore it from backup rather than deleting it.",
            ),
            Self::StateSaveFailed { .. } => {
                Some("Treat the operation as NOT applied and re-run it.")
            }
            _ => None,
        }
    }
}

// =============================================================================
// State codec (config-KV blob)
// =============================================================================

/// Decode the persisted managed-connector snapshot.
///
/// A malformed blob fails closed ([`RobotConnectorError::StateLoadFailed`])
/// — silently starting empty would resurrect uninstalled/disabled
/// connectors and forget installed ones.
pub fn decode_connector_state(json: &str) -> Result<Vec<ManagedConnector>, RobotConnectorError> {
    serde_json::from_str(json).map_err(|err| RobotConnectorError::StateLoadFailed {
        detail: err.to_string(),
    })
}

/// Encode the managed-connector snapshot for persistence.
pub fn encode_connector_state(
    connectors: &[ManagedConnector],
) -> Result<String, RobotConnectorError> {
    serde_json::to_string(connectors).map_err(|err| RobotConnectorError::StateSaveFailed {
        detail: err.to_string(),
    })
}

// =============================================================================
// Handler
// =============================================================================

/// Execute one connector-family action against a (rehydrated) engine.
///
/// Storage-free: callers load/persist the snapshot around this call. All
/// mutations route through [`PolicyEngine::run_connector_lifecycle_intent`];
/// the kill-switch and approval gates here are handler-level typed fronts
/// for the same fail-closed posture (the boundary's own kill-switch check
/// remains as defense in depth).
pub fn handle_connector_action(
    engine: &mut PolicyEngine,
    action: ConnectorAction,
    now_ms: u64,
) -> Result<ConnectorActionOutcome, RobotConnectorError> {
    let kill_switch_emergency = engine.quarantine_registry().kill_switch().is_emergency();

    // ── status: read-only ────────────────────────────────────────────────
    if let ConnectorAction::Status { connector_id } = &action {
        let manager = engine.lifecycle_manager();
        let connectors = match connector_id {
            Some(id) => {
                let mc = manager
                    .get(id)
                    .ok_or_else(|| RobotConnectorError::NotFound {
                        connector_id: id.clone(),
                    })?;
                vec![ConnectorStatusEntry::from_managed(mc)]
            }
            None => manager
                .managed_connectors()
                .iter()
                .map(ConnectorStatusEntry::from_managed)
                .collect(),
        };
        return Ok(ConnectorActionOutcome::Status(ConnectorStatusData {
            connectors,
            kill_switch_emergency,
            op_counter: manager.op_counter(),
            // Filled in by the CLI dispatch, which knows whether a blob
            // was present at load time.
            state_persisted: false,
        }));
    }

    let operation = action.op_name().to_string();
    let connector_id = action.target_connector_id().unwrap_or_default().to_string();

    // ── dry-run: side-effect-free plan receipt ───────────────────────────
    if action.dry_run() {
        let current = engine.lifecycle_manager().get(&connector_id);
        return Ok(ConnectorActionOutcome::DryRun(ConnectorDryRunData {
            connector_id,
            operation,
            dry_run: true,
            currently_installed: current.is_some(),
            current_admin_state: current.map(|mc| mc.admin_state.as_str().to_string()),
            would_require_approval: action.is_destructive(),
            kill_switch_emergency,
        }));
    }

    // ── approval gate: destructive intents are approval-blocked ─────────
    if action.is_destructive() {
        return Err(RobotConnectorError::RequireApproval {
            operation,
            connector_id,
        });
    }

    // ── kill switch: typed fail-closed front ─────────────────────────────
    if engine.authorize_connector_lifecycle_kill_switch(&operation, &connector_id, now_ms).is_err() {
        return Err(RobotConnectorError::KillSwitchActive);
    }

    // ── typed prechecks (read-only) ───────────────────────────────────────
    let intent = match action {
        ConnectorAction::Install { manifest, .. } => {
            if let Some(existing) = engine.lifecycle_manager().get(&manifest.package_id) {
                return Err(RobotConnectorError::AlreadyInstalled {
                    connector_id: manifest.package_id.clone(),
                    version: existing.version.clone(),
                });
            }
            manifest
                .validate()
                .map_err(|err| RobotConnectorError::ManifestInvalid {
                    detail: err.to_string(),
                })?;
            LifecycleIntent::Install {
                manifest: *manifest,
            }
        }
        ConnectorAction::Update {
            connector_id,
            manifest,
            ..
        } => {
            if engine.lifecycle_manager().get(&connector_id).is_none() {
                return Err(RobotConnectorError::NotFound { connector_id });
            }
            manifest
                .validate()
                .map_err(|err| RobotConnectorError::ManifestInvalid {
                    detail: err.to_string(),
                })?;
            LifecycleIntent::Update {
                connector_id,
                manifest: *manifest,
            }
        }
        ConnectorAction::Enable { connector_id, .. } => {
            if engine.lifecycle_manager().get(&connector_id).is_none() {
                return Err(RobotConnectorError::NotFound { connector_id });
            }
            LifecycleIntent::Enable { connector_id }
        }
        ConnectorAction::Disable {
            connector_id,
            reason,
            ..
        } => {
            if engine.lifecycle_manager().get(&connector_id).is_none() {
                return Err(RobotConnectorError::NotFound { connector_id });
            }
            LifecycleIntent::Disable {
                connector_id,
                reason,
            }
        }
        ConnectorAction::Restart { connector_id, .. } => {
            if engine.lifecycle_manager().get(&connector_id).is_none() {
                return Err(RobotConnectorError::NotFound { connector_id });
            }
            LifecycleIntent::Restart { connector_id }
        }
        ConnectorAction::Status { .. }
        | ConnectorAction::Uninstall { .. }
        | ConnectorAction::Rollback { .. } => {
            // Unreachable by construction (status returns early above;
            // destructive intents hit the approval gate), but kept
            // panic-free per the workspace no-panic discipline.
            return Err(RobotConnectorError::LifecycleFailed {
                detail: "internal: action variant handled earlier in dispatch".to_string(),
            });
        }
    };

    // ── the gated production boundary ─────────────────────────────────────
    let result = engine
        .run_connector_lifecycle_intent(intent, now_ms)
        .map_err(|detail| RobotConnectorError::LifecycleFailed { detail })?;

    Ok(ConnectorActionOutcome::Mutated(ConnectorMutationData {
        connector_id: result.connector_id,
        operation: result.operation,
        dry_run: false,
        success: result.success,
        admin_state: result.admin_state.as_str().to_string(),
        runtime_phase: result.runtime_phase.as_str().to_string(),
        detail: result.detail,
        at_ms: result.at_ms,
        persisted: false,
    }))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_host_runtime::ConnectorCapability;
    use crate::connector_lifecycle::ConnectorLifecycleManager;
    use crate::policy_quarantine::KillSwitchLevel;

    fn test_manifest(package_id: &str) -> ConnectorManifest {
        ConnectorManifest {
            schema_version: 1,
            package_id: package_id.to_string(),
            version: "1.0.0".to_string(),
            display_name: "Test Connector".to_string(),
            description: "test".to_string(),
            author: "test".to_string(),
            min_ft_version: None,
            sha256_digest: "a".repeat(64),
            required_capabilities: vec![ConnectorCapability::Invoke],
            publisher_signature: Some("sig".to_string()),
            transparency_token: None,
            created_at_ms: 1000,
            metadata: std::collections::BTreeMap::new(),
        }
    }

    fn install(engine: &mut PolicyEngine, id: &str) {
        let outcome = handle_connector_action(
            engine,
            ConnectorAction::Install {
                manifest: Box::new(test_manifest(id)),
                dry_run: false,
            },
            1000,
        )
        .expect("install should succeed");
        assert!(
            matches!(outcome, ConnectorActionOutcome::Mutated(ref m) if m.success),
            "install outcome: {outcome:?}"
        );
    }

    #[test]
    fn install_then_status_and_enable_disable_round_trip() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");

        let status = handle_connector_action(
            &mut engine,
            ConnectorAction::Status { connector_id: None },
            2000,
        )
        .expect("status");
        let ConnectorActionOutcome::Status(data) = status else {
            panic!("expected status data");
        };
        assert_eq!(data.connectors.len(), 1);
        assert_eq!(data.connectors[0].connector_id, "slack");
        assert_eq!(data.connectors[0].admin_state, "enabled");
        assert!(data.op_counter >= 1, "boundary must advance telemetry");

        let disabled = handle_connector_action(
            &mut engine,
            ConnectorAction::Disable {
                connector_id: "slack".to_string(),
                reason: "maintenance".to_string(),
                dry_run: false,
            },
            3000,
        )
        .expect("disable");
        assert!(
            matches!(disabled, ConnectorActionOutcome::Mutated(ref m) if m.admin_state == "disabled")
        );

        let enabled = handle_connector_action(
            &mut engine,
            ConnectorAction::Enable {
                connector_id: "slack".to_string(),
                dry_run: false,
            },
            4000,
        )
        .expect("enable");
        assert!(
            matches!(enabled, ConnectorActionOutcome::Mutated(ref m) if m.admin_state == "enabled")
        );
    }

    #[test]
    fn status_unknown_id_is_typed_not_found() {
        let mut engine = PolicyEngine::permissive();
        let err = handle_connector_action(
            &mut engine,
            ConnectorAction::Status {
                connector_id: Some("ghost".to_string()),
            },
            1000,
        )
        .expect_err("unknown id must be typed");
        assert_eq!(err.code(), "robot.connector.not_found");
    }

    #[test]
    fn dry_run_is_pure_and_flags_destructive_approval() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");
        let ops_before = engine.lifecycle_manager().op_counter();

        let receipt = handle_connector_action(
            &mut engine,
            ConnectorAction::Uninstall {
                connector_id: "slack".to_string(),
                dry_run: true,
            },
            2000,
        )
        .expect("dry-run uninstall receipt");
        let ConnectorActionOutcome::DryRun(data) = receipt else {
            panic!("expected dry-run receipt");
        };
        assert!(data.currently_installed);
        assert_eq!(data.current_admin_state.as_deref(), Some("enabled"));
        assert!(data.would_require_approval);

        // Purity: no manager mutation, no telemetry advance.
        assert_eq!(engine.lifecycle_manager().op_counter(), ops_before);
        assert!(engine.lifecycle_manager().get("slack").is_some());

        // Non-destructive dry-run does not require approval.
        let receipt = handle_connector_action(
            &mut engine,
            ConnectorAction::Enable {
                connector_id: "slack".to_string(),
                dry_run: true,
            },
            2000,
        )
        .expect("dry-run enable receipt");
        let ConnectorActionOutcome::DryRun(data) = receipt else {
            panic!("expected dry-run receipt");
        };
        assert!(!data.would_require_approval);
        assert_eq!(engine.lifecycle_manager().op_counter(), ops_before);
    }

    #[test]
    fn destructive_non_dry_run_requires_approval_without_touching_manager() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");
        let ops_before = engine.lifecycle_manager().op_counter();

        for action in [
            ConnectorAction::Uninstall {
                connector_id: "slack".to_string(),
                dry_run: false,
            },
            ConnectorAction::Rollback {
                connector_id: "slack".to_string(),
                dry_run: false,
            },
        ] {
            let err = handle_connector_action(&mut engine, action, 2000)
                .expect_err("destructive intents are approval-blocked");
            assert_eq!(err.code(), "robot.connector.require_approval");
        }
        assert_eq!(engine.lifecycle_manager().op_counter(), ops_before);
        assert!(engine.lifecycle_manager().get("slack").is_some());
    }

    #[test]
    fn kill_switch_fails_closed_with_typed_code() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");
        assert!(engine.quarantine_registry_mut().trip_kill_switch(
            KillSwitchLevel::EmergencyHalt,
            "test",
            "drill",
            5000,
        ));
        let ops_before = engine.lifecycle_manager().op_counter();

        let err = handle_connector_action(
            &mut engine,
            ConnectorAction::Disable {
                connector_id: "slack".to_string(),
                reason: "should not apply".to_string(),
                dry_run: false,
            },
            6000,
        )
        .expect_err("kill switch must fail closed");
        assert_eq!(err.code(), "robot.connector.kill_switch_active");
        assert_eq!(engine.lifecycle_manager().op_counter(), ops_before);
        assert_eq!(
            engine
                .lifecycle_manager()
                .get("slack")
                .map(|mc| mc.admin_state.as_str()),
            Some("enabled"),
            "denied intent must not perturb the manager"
        );

        // Dry-run receipts still render under the kill switch, flagged.
        let receipt = handle_connector_action(
            &mut engine,
            ConnectorAction::Enable {
                connector_id: "slack".to_string(),
                dry_run: true,
            },
            6000,
        )
        .expect("dry-run renders under kill switch");
        let ConnectorActionOutcome::DryRun(data) = receipt else {
            panic!("expected dry-run receipt");
        };
        assert!(data.kill_switch_emergency);
    }

    #[test]
    fn graduated_stop_lifecycle_handler_blocks_without_state_or_counter_changes() {
        for level in [KillSwitchLevel::HardStop, KillSwitchLevel::EmergencyHalt] {
            let mut engine = PolicyEngine::permissive();
            install(&mut engine, "slack");
            engine.trip_kill_switch(level, "operator", "drill", 2000);
            let before = encode_connector_state(&engine.lifecycle_manager().managed_connectors()).unwrap();
            let count = engine.lifecycle_manager().op_counter();
            let audit_before = engine.audit_chain().len();
            for action in [
                ConnectorAction::Install { manifest: Box::new(test_manifest("github")), dry_run: false },
                ConnectorAction::Update { connector_id: "slack".into(), manifest: Box::new(test_manifest("slack")), dry_run: false },
                ConnectorAction::Enable { connector_id: "slack".into(), dry_run: false },
                ConnectorAction::Restart { connector_id: "slack".into(), dry_run: false },
                ConnectorAction::Disable { connector_id: "slack".into(), reason: "unapproved recovery".into(), dry_run: false },
            ] {
                let error = handle_connector_action(&mut engine, action, 3000).expect_err("stopped lifecycle mutation");
                assert_eq!(error.code(), "robot.connector.kill_switch_active");
                assert_eq!(engine.lifecycle_manager().op_counter(), count);
                assert_eq!(encode_connector_state(&engine.lifecycle_manager().managed_connectors()).unwrap(), before);
            }
            assert_eq!(engine.audit_chain().len(), audit_before + 5);
            // The public lower-level production boundary must enforce the
            // identical gate even when the typed robot front is not involved.
            engine.run_connector_lifecycle_intent(LifecycleIntent::Enable { connector_id: "slack".into() }, 3000)
                .expect_err("direct boundary must deny too");
            assert_eq!(engine.lifecycle_manager().op_counter(), count);
            engine.quarantine_registry_mut().reset_kill_switch("operator", 4000);
            let recovered = handle_connector_action(&mut engine, ConnectorAction::Disable {
                connector_id: "slack".into(), reason: "authorized recovery after reset".into(), dry_run: false,
            }, 5000).expect("reset authorizes a fresh recovery action");
            assert!(matches!(recovered, ConnectorActionOutcome::Mutated(ref result) if result.success && result.admin_state == "disabled"));
            assert_eq!(engine.lifecycle_manager().op_counter(), count + 1);
        }
    }

    #[test]
    fn corrupt_persisted_switch_blocks_lifecycle_and_soft_stop_allows_admin_drain() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");
        let before = encode_connector_state(&engine.lifecycle_manager().managed_connectors()).unwrap();
        crate::policy_kill_switch_state::apply_persisted_kill_switch(&mut engine, Ok(Some("{broken".into())), 2000);
        assert!(matches!(handle_connector_action(&mut engine, ConnectorAction::Enable { connector_id: "slack".into(), dry_run: false }, 3000), Err(RobotConnectorError::KillSwitchActive)));
        assert_eq!(encode_connector_state(&engine.lifecycle_manager().managed_connectors()).unwrap(), before);
        engine.quarantine_registry_mut().reset_kill_switch("operator", 4000);
        engine.trip_kill_switch(KillSwitchLevel::SoftStop, "operator", "drain", 4001);
        let outcome = handle_connector_action(&mut engine, ConnectorAction::Disable { connector_id: "slack".into(), reason: "drain".into(), dry_run: false }, 5000).unwrap();
        assert!(matches!(outcome, ConnectorActionOutcome::Mutated(ref result) if result.success && result.admin_state == "disabled"));
    }

    #[test]
    fn install_duplicate_and_mutate_missing_are_typed() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");

        let err = handle_connector_action(
            &mut engine,
            ConnectorAction::Install {
                manifest: Box::new(test_manifest("slack")),
                dry_run: false,
            },
            2000,
        )
        .expect_err("duplicate install");
        assert_eq!(err.code(), "robot.connector.already_installed");

        let err = handle_connector_action(
            &mut engine,
            ConnectorAction::Enable {
                connector_id: "ghost".to_string(),
                dry_run: false,
            },
            2000,
        )
        .expect_err("enable on missing connector");
        assert_eq!(err.code(), "robot.connector.not_found");
    }

    #[test]
    fn state_codec_round_trip_and_rehydration() {
        let mut engine = PolicyEngine::permissive();
        install(&mut engine, "slack");
        install(&mut engine, "github");

        let snapshot = engine.lifecycle_manager().managed_connectors();
        let json = encode_connector_state(&snapshot).expect("encode");
        let restored = decode_connector_state(&json).expect("decode");
        assert_eq!(restored, snapshot);

        // Rehydrate into a fresh manager: same state, no telemetry advance.
        let mut fresh = ConnectorLifecycleManager::new(Default::default());
        fresh.restore_connectors(restored);
        assert_eq!(fresh.count(), 2);
        assert_eq!(fresh.op_counter(), 0);
        assert_eq!(
            fresh.get("slack").map(|mc| mc.admin_state.as_str()),
            Some("enabled")
        );
    }

    #[test]
    fn corrupt_state_blob_fails_closed() {
        let err = decode_connector_state("{ not json ]").expect_err("corrupt blob");
        assert_eq!(err.code(), "robot.connector.state_load_failed");
    }
}
