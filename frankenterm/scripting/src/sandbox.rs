//! Sandbox configuration and enforcement for WASM extensions.
//!
//! Combines the permission model ([`ExtensionPermissions`]) with
//! runtime resource limits (fuel, memory) and an [`AuditTrail`]
//! to form a complete security boundary around each extension.

use crate::audit::{AuditOutcome, AuditTrail};
use crate::manifest::ExtensionPermissions;
use anyhow::{Result, bail};
use std::sync::Arc;
use std::time::Duration;

/// Runtime resource limits for a sandboxed WASM extension.
#[derive(Clone, Debug)]
pub struct ResourceLimits {
    /// Maximum linear memory in bytes (default: 64 MiB).
    pub max_memory_bytes: usize,
    /// Fuel budget per host-to-WASM call (default: 1 billion).
    pub fuel_per_call: u64,
    /// Wall-clock timeout per call (default: 10s).
    pub max_wall_time: Duration,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            fuel_per_call: 1_000_000_000,
            max_wall_time: Duration::from_secs(10),
        }
    }
}

/// Complete sandbox configuration for one extension.
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// Extension identifier (from manifest).
    pub extension_id: String,
    /// Declared permissions.
    pub permissions: ExtensionPermissions,
    /// Resource limits.
    pub limits: ResourceLimits,
}

impl SandboxConfig {
    /// Create from manifest permissions with default resource limits.
    pub fn from_permissions(extension_id: String, permissions: ExtensionPermissions) -> Self {
        Self {
            extension_id,
            permissions,
            limits: ResourceLimits::default(),
        }
    }

    /// Override resource limits.
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Runtime sandbox enforcer that checks permissions and records audit entries.
pub struct SandboxEnforcer {
    config: SandboxConfig,
    audit: Arc<AuditTrail>,
}

impl SandboxEnforcer {
    /// Create a new enforcer with the given config and shared audit trail.
    pub fn new(config: SandboxConfig, audit: Arc<AuditTrail>) -> Self {
        Self { config, audit }
    }

    /// Check whether reading the given path is allowed.
    pub fn check_read(&self, path: &str) -> Result<()> {
        if self.config.permissions.allows_read(path) {
            self.audit
                .record(&self.config.extension_id, "fs_read", path, AuditOutcome::Ok);
            Ok(())
        } else {
            let reason = format!("read access denied for {path}");
            self.audit.record(
                &self.config.extension_id,
                "fs_read",
                path,
                AuditOutcome::Denied(reason.clone()),
            );
            bail!(reason)
        }
    }

    /// Check whether writing to the given path is allowed.
    pub fn check_write(&self, path: &str) -> Result<()> {
        if self.config.permissions.allows_write(path) {
            self.audit.record(
                &self.config.extension_id,
                "fs_write",
                path,
                AuditOutcome::Ok,
            );
            Ok(())
        } else {
            let reason = format!("write access denied for {path}");
            self.audit.record(
                &self.config.extension_id,
                "fs_write",
                path,
                AuditOutcome::Denied(reason.clone()),
            );
            bail!(reason)
        }
    }

    /// Check whether accessing the given environment variable is allowed.
    pub fn check_env_var(&self, name: &str) -> Result<()> {
        if self.config.permissions.allows_env_var(name) {
            self.audit
                .record(&self.config.extension_id, "env_get", name, AuditOutcome::Ok);
            Ok(())
        } else {
            let reason = format!("environment variable access denied for {name}");
            self.audit.record(
                &self.config.extension_id,
                "env_get",
                name,
                AuditOutcome::Denied(reason.clone()),
            );
            bail!(reason)
        }
    }

    /// Check whether network access is allowed.
    pub fn check_network(&self) -> Result<()> {
        if self.config.permissions.network {
            self.audit
                .record(&self.config.extension_id, "network", "", AuditOutcome::Ok);
            Ok(())
        } else {
            let reason = "network access denied".to_string();
            self.audit.record(
                &self.config.extension_id,
                "network",
                "",
                AuditOutcome::Denied(reason.clone()),
            );
            bail!(reason)
        }
    }

    /// Check whether pane content access is allowed.
    pub fn check_pane_access(&self, pane_id: u64) -> Result<()> {
        let args = format!("pane_id={pane_id}");
        if self.config.permissions.pane_access {
            self.audit.record(
                &self.config.extension_id,
                "pane_access",
                &args,
                AuditOutcome::Ok,
            );
            Ok(())
        } else {
            let reason = "pane content access denied".to_string();
            self.audit.record(
                &self.config.extension_id,
                "pane_access",
                &args,
                AuditOutcome::Denied(reason.clone()),
            );
            bail!(reason)
        }
    }

    /// Record a generic host function call.
    pub fn record_call(&self, function: &str, args: &str, outcome: AuditOutcome) {
        self.audit
            .record(&self.config.extension_id, function, args, outcome);
    }

    /// Get a reference to the underlying config.
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// Get a reference to the shared audit trail.
    pub fn audit_trail(&self) -> &Arc<AuditTrail> {
        &self.audit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_enforcer(perms: ExtensionPermissions) -> SandboxEnforcer {
        let config = SandboxConfig::from_permissions("test-ext".to_string(), perms);
        let audit = Arc::new(AuditTrail::new(1000));
        SandboxEnforcer::new(config, audit)
    }

    #[test]
    fn read_allowed_when_permitted() {
        let enforcer = test_enforcer(ExtensionPermissions {
            filesystem_read: vec!["~/.config/frankenterm/".to_string()],
            ..Default::default()
        });
        assert!(
            enforcer
                .check_read("~/.config/frankenterm/theme.toml")
                .is_ok()
        );
    }

    #[test]
    fn read_denied_when_not_permitted() {
        let enforcer = test_enforcer(ExtensionPermissions::default());
        assert!(enforcer.check_read("~/.ssh/id_rsa").is_err());
    }

    #[test]
    fn write_denied_by_default() {
        let enforcer = test_enforcer(ExtensionPermissions::default());
        assert!(enforcer.check_write("/tmp/data").is_err());
    }

    #[test]
    fn env_var_allowed_with_glob() {
        let enforcer = test_enforcer(ExtensionPermissions {
            environment: vec!["TERM".to_string(), "FRANKENTERM_*".to_string()],
            ..Default::default()
        });
        assert!(enforcer.check_env_var("TERM").is_ok());
        assert!(enforcer.check_env_var("FRANKENTERM_CONFIG").is_ok());
        assert!(enforcer.check_env_var("HOME").is_err());
    }

    #[test]
    fn network_denied_by_default() {
        let enforcer = test_enforcer(ExtensionPermissions::default());
        assert!(enforcer.check_network().is_err());
    }

    #[test]
    fn network_allowed_when_permitted() {
        let enforcer = test_enforcer(ExtensionPermissions {
            network: true,
            ..Default::default()
        });
        assert!(enforcer.check_network().is_ok());
    }

    #[test]
    fn pane_access_denied_by_default() {
        let enforcer = test_enforcer(ExtensionPermissions::default());
        assert!(enforcer.check_pane_access(42).is_err());
    }

    #[test]
    fn audit_trail_records_all_calls() {
        let enforcer = test_enforcer(ExtensionPermissions {
            filesystem_read: vec!["/allowed/".to_string()],
            ..Default::default()
        });
        let _ = enforcer.check_read("/allowed/file.txt");
        let _ = enforcer.check_read("/denied/file.txt");
        let _ = enforcer.check_network();

        let trail = enforcer.audit_trail();
        assert_eq!(trail.len(), 3);
        assert_eq!(trail.denied_count(), 2);
    }

    #[test]
    fn resource_limits_default() {
        let limits = ResourceLimits::default();
        assert_eq!(limits.max_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(limits.fuel_per_call, 1_000_000_000);
        assert_eq!(limits.max_wall_time, Duration::from_secs(10));
    }

    #[test]
    fn sandbox_config_with_custom_limits() {
        let config =
            SandboxConfig::from_permissions("ext".to_string(), ExtensionPermissions::default())
                .with_limits(ResourceLimits {
                    max_memory_bytes: 128 * 1024 * 1024,
                    fuel_per_call: 500_000_000,
                    max_wall_time: Duration::from_secs(5),
                });

        assert_eq!(config.limits.max_memory_bytes, 128 * 1024 * 1024);
        assert_eq!(config.limits.fuel_per_call, 500_000_000);
    }
}
