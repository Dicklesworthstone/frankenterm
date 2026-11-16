//! GUI plugin-host integration seam for WIT-bound WASM plugins.
//!
//! The concrete wasmtime component instantiation layer can stay small
//! because this module owns the deterministic host policy: manifest
//! validation, sandbox preopen planning, lifecycle transitions,
//! capability grants, resource-limit failures, and doctor/audit
//! snapshots.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use frankenterm_core::plugin_capabilities::{
    ApprovalToken, CapabilityGrantDecision, ManifestValidationError, PluginCapability,
    PluginLifecycleState, PluginLoaderPolicy, PluginManifest, PluginResourceBudget,
    validate_manifest,
};

pub const PLUGIN_WIT_CONTRACT: &str = r#"package frankenterm:plugin;

interface plugin {
    record state {
        pane-count: u32,
        active-pane-title: string,
        now-ms: u64,
    }

    record event {
        kind: string,
        payload: string,
    }

    enum lifecycle-stage {
        init,
        reload,
        shutdown,
    }

    render: func(state: state) -> string;
    handle-event: func(event: event);
    lifecycle: func(stage: lifecycle-stage);
    declare-capabilities: func() -> list<string>;
}
"#;

const WASMTIME_FUEL_UNITS_PER_MS: u64 = 10_000;
const GUEST_SANDBOX_PATH: &str = "/plugin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiPluginHostConfig {
    pub plugin_root: PathBuf,
    pub loader_policy: PluginLoaderPolicy,
}

impl GuiPluginHostConfig {
    #[must_use]
    pub fn new(plugin_root: impl Into<PathBuf>, loader_policy: PluginLoaderPolicy) -> Self {
        Self {
            plugin_root: plugin_root.into(),
            loader_policy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginLoadRequest {
    pub wasm_path: PathBuf,
    pub manifest: PluginManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasiPreopenPlan {
    pub host_path: PathBuf,
    pub guest_path: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmtimeRenderPlan {
    pub plugin_id: String,
    pub wasm_path: PathBuf,
    pub wasi_preopen: WasiPreopenPlan,
    pub fuel_units: u64,
    pub memory_cap_bytes: u64,
    pub disk_cap_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedGuiPlugin {
    pub manifest: PluginManifest,
    pub wasm_path: PathBuf,
    pub sandbox_dir: PathBuf,
    pub state: PluginLifecycleState,
    pub restart_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostDoctorSnapshot {
    pub loaded_plugins: usize,
    pub render_eligible_plugins: usize,
    pub errored_plugins: usize,
    pub audit_events: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginHostAuditEvent {
    pub plugin_id: String,
    pub kind: PluginHostAuditKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginHostAuditKind {
    Loaded,
    LifecycleTransition {
        from: PluginLifecycleState,
        to: PluginLifecycleState,
    },
    CapabilityDecision {
        capability: PluginCapability,
        decision: CapabilityGrantDecision,
    },
    ResourceLimitExceeded {
        limit: ResourceLimitKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimitKind {
    RenderDeadline,
    Memory,
    Disk,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginHostError {
    Manifest(ManifestValidationError),
    InvalidPluginId {
        plugin_id: String,
    },
    DuplicatePlugin {
        plugin_id: String,
    },
    UnknownPlugin {
        plugin_id: String,
    },
    InvalidLifecycleTransition {
        plugin_id: String,
        from: PluginLifecycleState,
        to: PluginLifecycleState,
    },
    CapabilityDenied {
        plugin_id: String,
        capability: PluginCapability,
        decision: CapabilityGrantDecision,
    },
    ResourceLimitExceeded {
        plugin_id: String,
        limit: ResourceLimitKind,
    },
}

#[derive(Debug, Clone)]
pub struct GuiPluginHost {
    config: GuiPluginHostConfig,
    plugins: BTreeMap<String, LoadedGuiPlugin>,
    audit: Vec<PluginHostAuditEvent>,
}

impl GuiPluginHost {
    #[must_use]
    pub fn new(config: GuiPluginHostConfig) -> Self {
        Self {
            config,
            plugins: BTreeMap::new(),
            audit: Vec::new(),
        }
    }

    pub fn load(&mut self, request: PluginLoadRequest) -> Result<(), PluginHostError> {
        validate_manifest(&request.manifest, self.config.loader_policy)
            .map_err(PluginHostError::Manifest)?;
        let plugin_id = request.manifest.id.clone();
        if self.plugins.contains_key(&plugin_id) {
            return Err(PluginHostError::DuplicatePlugin { plugin_id });
        }

        let sandbox_dir = sandbox_dir_for_plugin(&self.config.plugin_root, &plugin_id)?;
        let loaded = LoadedGuiPlugin {
            manifest: request.manifest,
            wasm_path: request.wasm_path,
            sandbox_dir,
            state: PluginLifecycleState::Loaded,
            restart_attempts: 0,
        };
        self.plugins.insert(plugin_id.clone(), loaded);
        self.audit.push(PluginHostAuditEvent {
            plugin_id,
            kind: PluginHostAuditKind::Loaded,
        });
        Ok(())
    }

    pub fn transition(
        &mut self,
        plugin_id: &str,
        next: PluginLifecycleState,
    ) -> Result<(), PluginHostError> {
        let plugin =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| PluginHostError::UnknownPlugin {
                    plugin_id: plugin_id.to_string(),
                })?;
        let from = plugin.state;
        if !from.can_transition_to(next) {
            return Err(PluginHostError::InvalidLifecycleTransition {
                plugin_id: plugin_id.to_string(),
                from,
                to: next,
            });
        }
        plugin.state = next;
        self.audit.push(PluginHostAuditEvent {
            plugin_id: plugin_id.to_string(),
            kind: PluginHostAuditKind::LifecycleTransition { from, to: next },
        });
        Ok(())
    }

    pub fn request_capability(
        &mut self,
        plugin_id: &str,
        capability: PluginCapability,
        approval_tokens: &[ApprovalToken],
    ) -> Result<(), PluginHostError> {
        let manifest = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| PluginHostError::UnknownPlugin {
                plugin_id: plugin_id.to_string(),
            })?
            .manifest
            .clone();
        let decision = frankenterm_core::plugin_capabilities::decide_capability_grant(
            &manifest,
            approval_tokens,
            capability,
        );
        self.audit.push(PluginHostAuditEvent {
            plugin_id: plugin_id.to_string(),
            kind: PluginHostAuditKind::CapabilityDecision {
                capability,
                decision: decision.clone(),
            },
        });
        if decision == CapabilityGrantDecision::Granted {
            Ok(())
        } else {
            Err(PluginHostError::CapabilityDenied {
                plugin_id: plugin_id.to_string(),
                capability,
                decision,
            })
        }
    }

    pub fn render_plan(&self, plugin_id: &str) -> Result<WasmtimeRenderPlan, PluginHostError> {
        let plugin = self
            .plugins
            .get(plugin_id)
            .ok_or_else(|| PluginHostError::UnknownPlugin {
                plugin_id: plugin_id.to_string(),
            })?;
        let budget = plugin
            .manifest
            .resource_budget
            .clamp_to_safe_bounds()
            .budget;
        Ok(WasmtimeRenderPlan {
            plugin_id: plugin_id.to_string(),
            wasm_path: plugin.wasm_path.clone(),
            wasi_preopen: WasiPreopenPlan {
                host_path: plugin.sandbox_dir.clone(),
                guest_path: GUEST_SANDBOX_PATH,
            },
            fuel_units: fuel_for_budget(budget),
            memory_cap_bytes: budget.memory_cap_bytes,
            disk_cap_bytes: budget.disk_cap_bytes,
        })
    }

    pub fn record_resource_observation(
        &mut self,
        plugin_id: &str,
        render_elapsed_ms: u32,
        memory_bytes: u64,
        disk_bytes: u64,
    ) -> Result<(), PluginHostError> {
        let plugin =
            self.plugins
                .get_mut(plugin_id)
                .ok_or_else(|| PluginHostError::UnknownPlugin {
                    plugin_id: plugin_id.to_string(),
                })?;
        let budget = plugin
            .manifest
            .resource_budget
            .clamp_to_safe_bounds()
            .budget;
        let limit = if render_elapsed_ms > budget.render_deadline_ms {
            Some(ResourceLimitKind::RenderDeadline)
        } else if memory_bytes > budget.memory_cap_bytes {
            Some(ResourceLimitKind::Memory)
        } else if disk_bytes > budget.disk_cap_bytes {
            Some(ResourceLimitKind::Disk)
        } else {
            None
        };

        let Some(limit) = limit else {
            return Ok(());
        };
        plugin.state = PluginLifecycleState::Errored;
        plugin.restart_attempts = plugin.restart_attempts.saturating_add(1);
        self.audit.push(PluginHostAuditEvent {
            plugin_id: plugin_id.to_string(),
            kind: PluginHostAuditKind::ResourceLimitExceeded { limit },
        });
        Err(PluginHostError::ResourceLimitExceeded {
            plugin_id: plugin_id.to_string(),
            limit,
        })
    }

    #[must_use]
    pub fn doctor_snapshot(&self) -> PluginHostDoctorSnapshot {
        PluginHostDoctorSnapshot {
            loaded_plugins: self.plugins.len(),
            render_eligible_plugins: self
                .plugins
                .values()
                .filter(|plugin| plugin.state.is_render_eligible())
                .count(),
            errored_plugins: self
                .plugins
                .values()
                .filter(|plugin| plugin.state == PluginLifecycleState::Errored)
                .count(),
            audit_events: self.audit.len(),
        }
    }

    #[must_use]
    pub fn audit_events(&self) -> &[PluginHostAuditEvent] {
        &self.audit
    }
}

fn fuel_for_budget(budget: PluginResourceBudget) -> u64 {
    u64::from(budget.render_deadline_ms).saturating_mul(WASMTIME_FUEL_UNITS_PER_MS)
}

fn sandbox_dir_for_plugin(root: &Path, plugin_id: &str) -> Result<PathBuf, PluginHostError> {
    let path = Path::new(plugin_id);
    let valid = !plugin_id.is_empty()
        && !path.is_absolute()
        && path.components().all(|component| match component {
            Component::Normal(part) => part.to_str().is_some_and(valid_plugin_id_segment),
            _ => false,
        })
        && path.components().count() == 1;
    if !valid {
        return Err(PluginHostError::InvalidPluginId {
            plugin_id: plugin_id.to_string(),
        });
    }
    Ok(root.join(plugin_id))
}

fn valid_plugin_id_segment(segment: &str) -> bool {
    segment
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::plugin_capabilities::{
        ApprovalToken, HARD_DISK_CAP_BYTES, HARD_MEMORY_CAP_BYTES, HARD_RENDER_DEADLINE_CEILING_MS,
        PluginSignatureClass,
    };

    fn manifest(id: &str, caps: &[PluginCapability]) -> PluginManifest {
        PluginManifest::new(id, "1.0.0")
            .with_signature(PluginSignatureClass::SelfSigned)
            .with_capabilities(caps.iter().copied())
    }

    fn host() -> GuiPluginHost {
        GuiPluginHost::new(GuiPluginHostConfig::new(
            "/tmp/ft-plugins",
            PluginLoaderPolicy::default(),
        ))
    }

    #[test]
    fn wit_contract_exposes_required_plugin_functions() {
        for needle in [
            "render: func",
            "handle-event: func",
            "lifecycle: func",
            "declare-capabilities: func",
            "enum lifecycle-stage",
        ] {
            assert!(PLUGIN_WIT_CONTRACT.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn load_valid_manifest_plans_single_sandbox_preopen() {
        let mut host = host();
        host.load(PluginLoadRequest {
            wasm_path: PathBuf::from("/tmp/clock.wasm"),
            manifest: manifest(
                "clock-tile",
                &[PluginCapability::ReadState, PluginCapability::RenderTile],
            ),
        })
        .unwrap();

        let plan = host.render_plan("clock-tile").unwrap();
        assert_eq!(
            plan.wasi_preopen.host_path,
            PathBuf::from("/tmp/ft-plugins/clock-tile")
        );
        assert_eq!(plan.wasi_preopen.guest_path, "/plugin");
        assert_eq!(plan.fuel_units, 500_000);
        assert_eq!(plan.memory_cap_bytes, HARD_MEMORY_CAP_BYTES);
        assert_eq!(plan.disk_cap_bytes, HARD_DISK_CAP_BYTES);
    }

    #[test]
    fn load_rejects_sandbox_escape_plugin_id() {
        let mut host = host();
        let err = host
            .load(PluginLoadRequest {
                wasm_path: PathBuf::from("/tmp/evil.wasm"),
                manifest: manifest("../evil", &[PluginCapability::ReadState]),
            })
            .unwrap_err();
        assert_eq!(
            err,
            PluginHostError::InvalidPluginId {
                plugin_id: "../evil".into()
            }
        );
    }

    #[test]
    fn lifecycle_transitions_are_gated_by_substrate_table() {
        let mut host = host();
        host.load(PluginLoadRequest {
            wasm_path: PathBuf::from("/tmp/clock.wasm"),
            manifest: manifest("clock", &[PluginCapability::RenderTile]),
        })
        .unwrap();

        assert!(
            host.transition("clock", PluginLifecycleState::Initialised)
                .is_ok()
        );
        assert!(
            host.transition("clock", PluginLifecycleState::Running)
                .is_ok()
        );
        let err = host
            .transition("clock", PluginLifecycleState::Loaded)
            .unwrap_err();
        assert!(matches!(
            err,
            PluginHostError::InvalidLifecycleTransition {
                from: PluginLifecycleState::Running,
                to: PluginLifecycleState::Loaded,
                ..
            }
        ));
    }

    #[test]
    fn capability_gate_requires_approval_for_restricted_capability() {
        let mut host = host();
        host.load(PluginLoadRequest {
            wasm_path: PathBuf::from("/tmp/git.wasm"),
            manifest: manifest(
                "git-branch-tile",
                &[PluginCapability::ReadState, PluginCapability::Network],
            ),
        })
        .unwrap();

        let denied = host
            .request_capability("git-branch-tile", PluginCapability::Network, &[])
            .unwrap_err();
        assert!(matches!(
            denied,
            PluginHostError::CapabilityDenied {
                decision: CapabilityGrantDecision::DeniedNoApproval,
                ..
            }
        ));

        let token = ApprovalToken {
            plugin_id: "git-branch-tile".into(),
            capability: PluginCapability::Network,
        };
        assert!(
            host.request_capability("git-branch-tile", PluginCapability::Network, &[token])
                .is_ok()
        );
    }

    #[test]
    fn resource_limit_exceeded_moves_plugin_to_errored() {
        let mut host = host();
        host.load(PluginLoadRequest {
            wasm_path: PathBuf::from("/tmp/fleet.wasm"),
            manifest: manifest("fleet-status-tile", &[PluginCapability::RenderTile]),
        })
        .unwrap();

        let err = host
            .record_resource_observation(
                "fleet-status-tile",
                HARD_RENDER_DEADLINE_CEILING_MS + 1,
                0,
                0,
            )
            .unwrap_err();

        assert_eq!(
            err,
            PluginHostError::ResourceLimitExceeded {
                plugin_id: "fleet-status-tile".into(),
                limit: ResourceLimitKind::RenderDeadline,
            }
        );
        assert_eq!(host.doctor_snapshot().errored_plugins, 1);
    }
}
