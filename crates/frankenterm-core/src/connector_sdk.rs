//! Connector SDK / devkit and certification pipeline.
//!
//! Developer-facing tools for building, testing, validating, and certifying
//! FrankenTerm connectors. Provides:
//!
//! - **Builders**: Fluent APIs for constructing manifests, trust policies, and
//!   sandbox configurations with compile-time validation.
//! - **Linting**: Static analysis of connector manifests for common mistakes,
//!   capability over-request detection, and policy compliance checks.
//! - **Certification**: Multi-phase validation pipeline (schema → capabilities →
//!   signature → integration) with structured verdicts and remediation hints.
//! - **Local simulation**: In-memory connector lifecycle harness for rapid
//!   dev-test cycles without I/O.
//! - **Telemetry**: Counters and snapshots for SDK usage and certification
//!   pipeline health.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::connector_host_runtime::{
    ConnectorCapability, ConnectorCapabilityEnvelope, ConnectorHostConfig, ConnectorHostRuntime,
    ConnectorLifecyclePhase, ConnectorOperationRequest, ConnectorProtocolVersion,
    ConnectorSandboxZone, StartupProbeResult,
};
use crate::connector_registry::{
    ConnectorManifest, ConnectorRegistryClient, ConnectorRegistryConfig, TrustLevel, TrustPolicy,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LINT_HISTORY_CAPACITY: usize = 256;
const CERTIFICATION_HISTORY_CAPACITY: usize = 128;
const MAX_PACKAGE_ID_LEN: usize = 128;
const MAX_DISPLAY_NAME_LEN: usize = 256;
const MAX_DESCRIPTION_LEN: usize = 4096;
const MAX_AUTHOR_LEN: usize = 256;
const CURRENT_FT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Manifest metadata key required to probe `filesystem_read` capability claims.
pub const PROBE_METADATA_FILESYSTEM_READ_TARGET: &str = "certification.probe.fs_read_target";
/// Manifest metadata key required to probe `filesystem_write` capability claims.
pub const PROBE_METADATA_FILESYSTEM_WRITE_TARGET: &str = "certification.probe.fs_write_target";
/// Manifest metadata key required to probe `network_egress` capability claims.
pub const PROBE_METADATA_NETWORK_TARGET: &str = "certification.probe.network_target";
/// Manifest metadata key required to probe `process_exec` capability claims.
pub const PROBE_METADATA_EXEC_TARGET: &str = "certification.probe.exec_target";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the Connector SDK / certification pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ConnectorSdkError {
    #[error("manifest builder: {reason}")]
    ManifestBuilder { reason: String },

    #[error("policy builder: {reason}")]
    PolicyBuilder { reason: String },

    #[error("lint failure: {reason}")]
    LintFailure { reason: String },

    #[error("certification failure in phase {phase}: {reason}")]
    CertificationFailure { phase: String, reason: String },

    #[error("simulation error: {reason}")]
    SimulationError { reason: String },

    #[error("registry error: {reason}")]
    RegistryError { reason: String },
}

// ---------------------------------------------------------------------------
// Manifest builder
// ---------------------------------------------------------------------------

/// Fluent builder for `ConnectorManifest`.
///
/// ```ignore
/// let manifest = ManifestBuilder::new("my-connector")
///     .version("1.0.0")
///     .display_name("My Connector")
///     .author("dev@example.com")
///     .capability(ConnectorCapability::Invoke)
///     .capability(ConnectorCapability::ReadState)
///     .build_with_digest(payload_bytes)?;
/// ```
#[derive(Debug, Clone)]
pub struct ManifestBuilder {
    package_id: String,
    version: Option<String>,
    display_name: Option<String>,
    description: String,
    author: Option<String>,
    min_ft_version: Option<String>,
    required_capabilities: Vec<ConnectorCapability>,
    publisher_signature: Option<String>,
    schema_version: u32,
}

impl ManifestBuilder {
    /// Start building a manifest for the given package ID.
    #[must_use]
    pub fn new(package_id: impl Into<String>) -> Self {
        Self {
            package_id: package_id.into(),
            version: None,
            display_name: None,
            description: String::new(),
            author: None,
            min_ft_version: None,
            required_capabilities: Vec::new(),
            publisher_signature: None,
            schema_version: 1,
        }
    }

    /// Set the semantic version string.
    #[must_use]
    pub fn version(mut self, v: impl Into<String>) -> Self {
        self.version = Some(v.into());
        self
    }

    /// Set the human-readable display name.
    #[must_use]
    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    /// Set the description.
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Set the author.
    #[must_use]
    pub fn author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Set the minimum FrankenTerm version required.
    #[must_use]
    pub fn min_ft_version(mut self, v: impl Into<String>) -> Self {
        self.min_ft_version = Some(v.into());
        self
    }

    /// Add a required capability.
    #[must_use]
    pub fn capability(mut self, cap: ConnectorCapability) -> Self {
        if !self.required_capabilities.contains(&cap) {
            self.required_capabilities.push(cap);
        }
        self
    }

    /// Add multiple required capabilities.
    #[must_use]
    pub fn capabilities(mut self, caps: &[ConnectorCapability]) -> Self {
        for &cap in caps {
            if !self.required_capabilities.contains(&cap) {
                self.required_capabilities.push(cap);
            }
        }
        self
    }

    /// Set the publisher signature (hex-encoded).
    #[must_use]
    pub fn publisher_signature(mut self, sig: impl Into<String>) -> Self {
        self.publisher_signature = Some(sig.into());
        self
    }

    /// Set the schema version (defaults to 1).
    #[must_use]
    pub fn schema_version(mut self, v: u32) -> Self {
        self.schema_version = v;
        self
    }

    /// Build the manifest, computing the SHA-256 digest from the provided payload.
    pub fn build_with_digest(self, payload: &[u8]) -> Result<ConnectorManifest, ConnectorSdkError> {
        let package_id = if self.package_id.is_empty() {
            return Err(ConnectorSdkError::ManifestBuilder {
                reason: "package_id is required".to_string(),
            });
        } else {
            self.package_id
        };

        let version = self
            .version
            .ok_or_else(|| ConnectorSdkError::ManifestBuilder {
                reason: "version is required".to_string(),
            })?;

        let display_name = self.display_name.unwrap_or_else(|| package_id.clone());
        let author = self.author.unwrap_or_default();

        let sha256_digest = compute_sha256_hex(payload);

        Ok(ConnectorManifest {
            schema_version: self.schema_version,
            package_id,
            version,
            display_name,
            description: self.description,
            author,
            min_ft_version: self.min_ft_version,
            sha256_digest,
            required_capabilities: self.required_capabilities,
            publisher_signature: self.publisher_signature,
            transparency_token: None,
            created_at_ms: 0,
            metadata: std::collections::BTreeMap::new(),
        })
    }

    /// Build the manifest with a pre-computed digest (hex string).
    pub fn build_with_precomputed_digest(
        self,
        digest_hex: impl Into<String>,
    ) -> Result<ConnectorManifest, ConnectorSdkError> {
        let package_id = if self.package_id.is_empty() {
            return Err(ConnectorSdkError::ManifestBuilder {
                reason: "package_id is required".to_string(),
            });
        } else {
            self.package_id
        };

        let version = self
            .version
            .ok_or_else(|| ConnectorSdkError::ManifestBuilder {
                reason: "version is required".to_string(),
            })?;

        let display_name = self.display_name.unwrap_or_else(|| package_id.clone());
        let author = self.author.unwrap_or_default();

        Ok(ConnectorManifest {
            schema_version: self.schema_version,
            package_id,
            version,
            display_name,
            description: self.description,
            author,
            min_ft_version: self.min_ft_version,
            sha256_digest: digest_hex.into(),
            required_capabilities: self.required_capabilities,
            publisher_signature: self.publisher_signature,
            transparency_token: None,
            created_at_ms: 0,
            metadata: std::collections::BTreeMap::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Trust policy builder
// ---------------------------------------------------------------------------

/// Fluent builder for `TrustPolicy`.
#[derive(Debug, Clone)]
pub struct TrustPolicyBuilder {
    min_install_level: TrustLevel,
    require_signature: bool,
    require_transparency_proof: bool,
    max_allowed_capabilities: Vec<ConnectorCapability>,
    trusted_publishers: Vec<String>,
    trusted_transparency_roots: Vec<String>,
    blocked_packages: Vec<String>,
}

impl Default for TrustPolicyBuilder {
    fn default() -> Self {
        Self {
            min_install_level: TrustLevel::Conditional,
            require_signature: false,
            require_transparency_proof: false,
            max_allowed_capabilities: Vec::new(),
            trusted_publishers: Vec::new(),
            trusted_transparency_roots: Vec::new(),
            blocked_packages: Vec::new(),
        }
    }
}

impl TrustPolicyBuilder {
    /// Create a new builder with permissive defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a strict builder that requires signatures and high trust.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            min_install_level: TrustLevel::Trusted,
            require_signature: true,
            require_transparency_proof: true,
            ..Default::default()
        }
    }

    /// Set the minimum trust level for installation.
    #[must_use]
    pub fn min_install_level(mut self, level: TrustLevel) -> Self {
        self.min_install_level = level;
        self
    }

    /// Require publisher signatures.
    #[must_use]
    pub fn require_signature(mut self, val: bool) -> Self {
        self.require_signature = val;
        self
    }

    /// Require transparency log proofs.
    #[must_use]
    pub fn require_transparency_proof(mut self, val: bool) -> Self {
        self.require_transparency_proof = val;
        self
    }

    /// Allow a specific capability.
    #[must_use]
    pub fn allow_capability(mut self, cap: ConnectorCapability) -> Self {
        if !self.max_allowed_capabilities.contains(&cap) {
            self.max_allowed_capabilities.push(cap);
        }
        self
    }

    /// Allow multiple capabilities.
    #[must_use]
    pub fn allow_capabilities(mut self, caps: &[ConnectorCapability]) -> Self {
        for &cap in caps {
            if !self.max_allowed_capabilities.contains(&cap) {
                self.max_allowed_capabilities.push(cap);
            }
        }
        self
    }

    /// Add a trusted publisher.
    #[must_use]
    pub fn trusted_publisher(mut self, publisher: impl Into<String>) -> Self {
        self.trusted_publishers.push(publisher.into());
        self
    }

    /// Add an operator-trusted transparency root hash.
    #[must_use]
    pub fn trusted_transparency_root(mut self, root_hash: impl Into<String>) -> Self {
        self.trusted_transparency_roots.push(root_hash.into());
        self
    }

    /// Block a specific package ID.
    #[must_use]
    pub fn block_package(mut self, package_id: impl Into<String>) -> Self {
        self.blocked_packages.push(package_id.into());
        self
    }

    /// Build the trust policy.
    #[must_use]
    pub fn build(self) -> TrustPolicy {
        TrustPolicy {
            min_install_level: self.min_install_level,
            require_signature: self.require_signature,
            require_transparency_proof: self.require_transparency_proof,
            max_allowed_capabilities: self.max_allowed_capabilities,
            trusted_publishers: self.trusted_publishers,
            trusted_transparency_roots: self.trusted_transparency_roots,
            blocked_packages: self.blocked_packages,
        }
    }
}

// ---------------------------------------------------------------------------
// Sandbox config builder
// ---------------------------------------------------------------------------

/// Fluent builder for `ConnectorSandboxZone`.
#[derive(Debug, Clone)]
pub struct SandboxBuilder {
    zone_id: String,
    fail_closed: bool,
    allowed_capabilities: Vec<ConnectorCapability>,
    filesystem_read_prefixes: Vec<String>,
    filesystem_write_prefixes: Vec<String>,
    network_allow_hosts: Vec<String>,
    allowed_exec_commands: Vec<String>,
}

impl SandboxBuilder {
    /// Create a sandbox builder for the given zone ID.
    #[must_use]
    pub fn new(zone_id: impl Into<String>) -> Self {
        Self {
            zone_id: zone_id.into(),
            fail_closed: true,
            allowed_capabilities: Vec::new(),
            filesystem_read_prefixes: Vec::new(),
            filesystem_write_prefixes: Vec::new(),
            network_allow_hosts: Vec::new(),
            allowed_exec_commands: Vec::new(),
        }
    }

    /// Set fail-open (deny-by-default is the default; this overrides).
    #[must_use]
    pub fn fail_open(mut self) -> Self {
        self.fail_closed = false;
        self
    }

    /// Allow a capability in the sandbox.
    #[must_use]
    pub fn allow(mut self, cap: ConnectorCapability) -> Self {
        if !self.allowed_capabilities.contains(&cap) {
            self.allowed_capabilities.push(cap);
        }
        self
    }

    /// Add a filesystem read prefix.
    #[must_use]
    pub fn read_path(mut self, prefix: impl Into<String>) -> Self {
        self.filesystem_read_prefixes.push(prefix.into());
        self
    }

    /// Add a filesystem write prefix.
    #[must_use]
    pub fn write_path(mut self, prefix: impl Into<String>) -> Self {
        self.filesystem_write_prefixes.push(prefix.into());
        self
    }

    /// Allow network egress to a host.
    #[must_use]
    pub fn network_host(mut self, host: impl Into<String>) -> Self {
        self.network_allow_hosts.push(host.into());
        self
    }

    /// Allow a process execution command.
    #[must_use]
    pub fn exec_command(mut self, cmd: impl Into<String>) -> Self {
        self.allowed_exec_commands.push(cmd.into());
        self
    }

    /// Build the sandbox zone configuration.
    #[must_use]
    pub fn build(self) -> ConnectorSandboxZone {
        ConnectorSandboxZone {
            zone_id: self.zone_id,
            fail_closed: self.fail_closed,
            capability_envelope: ConnectorCapabilityEnvelope {
                allowed_capabilities: self.allowed_capabilities,
                filesystem_read_prefixes: self.filesystem_read_prefixes,
                filesystem_write_prefixes: self.filesystem_write_prefixes,
                network_allow_hosts: self.network_allow_hosts,
                allowed_exec_commands: self.allowed_exec_commands,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Lint rules and findings
// ---------------------------------------------------------------------------

/// Severity levels for lint findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LintSeverity {
    Info,
    Warning,
    Error,
}

impl LintSeverity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for LintSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A lint rule that checks a manifest for potential issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintRule {
    pub rule_id: String,
    pub severity: LintSeverity,
    pub description: String,
}

/// A single finding from a lint check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintFinding {
    pub rule_id: String,
    pub severity: LintSeverity,
    pub message: String,
    pub remediation: Option<String>,
}

impl std::fmt::Display for LintFinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}: {}", self.severity, self.rule_id, self.message)?;
        if let Some(ref fix) = self.remediation {
            write!(f, " (fix: {fix})")?;
        }
        Ok(())
    }
}

/// Aggregated lint report for a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LintReport {
    pub package_id: String,
    pub findings: Vec<LintFinding>,
    pub error_count: usize,
    pub warning_count: usize,
    pub info_count: usize,
}

impl LintReport {
    /// True if the report contains no errors.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.error_count == 0
    }

    /// True if the report contains no errors or warnings.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.error_count == 0 && self.warning_count == 0
    }
}

impl std::fmt::Display for LintReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "lint({}): {} error(s), {} warning(s), {} info",
            self.package_id, self.error_count, self.warning_count, self.info_count
        )
    }
}

// ---------------------------------------------------------------------------
// Manifest linter
// ---------------------------------------------------------------------------

/// Static analysis linter for connector manifests.
pub struct ManifestLinter {
    rules: Vec<LintRule>,
    policy: Option<TrustPolicy>,
    history: VecDeque<LintReport>,
}

impl ManifestLinter {
    /// Create a linter with default rules.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: default_lint_rules(),
            policy: None,
            history: VecDeque::with_capacity(LINT_HISTORY_CAPACITY),
        }
    }

    /// Create a linter that also checks against a trust policy.
    #[must_use]
    pub fn with_policy(policy: TrustPolicy) -> Self {
        Self {
            rules: default_lint_rules(),
            policy: Some(policy),
            history: VecDeque::with_capacity(LINT_HISTORY_CAPACITY),
        }
    }

    /// Lint a manifest, returning a structured report.
    pub fn lint(&mut self, manifest: &ConnectorManifest) -> LintReport {
        let mut findings = Vec::new();

        // Rule: package_id format
        if manifest.package_id.is_empty() {
            findings.push(LintFinding {
                rule_id: "sdk.lint.empty_package_id".to_string(),
                severity: LintSeverity::Error,
                message: "package_id must not be empty".to_string(),
                remediation: Some("Provide a non-empty package_id".to_string()),
            });
        } else if manifest.package_id.len() > MAX_PACKAGE_ID_LEN {
            findings.push(LintFinding {
                rule_id: "sdk.lint.package_id_too_long".to_string(),
                severity: LintSeverity::Error,
                message: format!(
                    "package_id exceeds {} characters (got {})",
                    MAX_PACKAGE_ID_LEN,
                    manifest.package_id.len()
                ),
                remediation: Some(format!(
                    "Shorten package_id to at most {MAX_PACKAGE_ID_LEN} characters"
                )),
            });
        } else if !manifest
            .package_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            findings.push(LintFinding {
                rule_id: "sdk.lint.package_id_invalid_chars".to_string(),
                severity: LintSeverity::Error,
                message: "package_id contains invalid characters".to_string(),
                remediation: Some(
                    "Use only ASCII alphanumeric, hyphens, underscores, and dots".to_string(),
                ),
            });
        }

        // Rule: version format (basic semver check)
        if manifest.version.is_empty() {
            findings.push(LintFinding {
                rule_id: "sdk.lint.empty_version".to_string(),
                severity: LintSeverity::Error,
                message: "version must not be empty".to_string(),
                remediation: Some("Provide a semantic version like 1.0.0".to_string()),
            });
        } else if !is_basic_semver(&manifest.version) {
            findings.push(LintFinding {
                rule_id: "sdk.lint.non_semver_version".to_string(),
                severity: LintSeverity::Warning,
                message: format!(
                    "version '{}' does not appear to be semantic versioning",
                    manifest.version
                ),
                remediation: Some("Use MAJOR.MINOR.PATCH format".to_string()),
            });
        }

        // Rule: display name length
        if manifest.display_name.len() > MAX_DISPLAY_NAME_LEN {
            findings.push(LintFinding {
                rule_id: "sdk.lint.display_name_too_long".to_string(),
                severity: LintSeverity::Warning,
                message: format!("display_name exceeds {} characters", MAX_DISPLAY_NAME_LEN),
                remediation: Some("Shorten display_name for readability".to_string()),
            });
        }

        // Rule: description length
        if manifest.description.len() > MAX_DESCRIPTION_LEN {
            findings.push(LintFinding {
                rule_id: "sdk.lint.description_too_long".to_string(),
                severity: LintSeverity::Warning,
                message: format!("description exceeds {} characters", MAX_DESCRIPTION_LEN),
                remediation: Some("Shorten description; move details to docs".to_string()),
            });
        }

        // Rule: author length
        if manifest.author.len() > MAX_AUTHOR_LEN {
            findings.push(LintFinding {
                rule_id: "sdk.lint.author_too_long".to_string(),
                severity: LintSeverity::Warning,
                message: format!("author exceeds {} characters", MAX_AUTHOR_LEN),
                remediation: None,
            });
        }

        // Rule: digest format
        if manifest.sha256_digest.len() != 64 {
            findings.push(LintFinding {
                rule_id: "sdk.lint.invalid_digest_length".to_string(),
                severity: LintSeverity::Error,
                message: format!(
                    "sha256_digest should be 64 hex chars, got {}",
                    manifest.sha256_digest.len()
                ),
                remediation: Some(
                    "Use ManifestBuilder::build_with_digest to auto-compute".to_string(),
                ),
            });
        } else if !manifest
            .sha256_digest
            .chars()
            .all(|c| c.is_ascii_hexdigit())
        {
            findings.push(LintFinding {
                rule_id: "sdk.lint.invalid_digest_chars".to_string(),
                severity: LintSeverity::Error,
                message: "sha256_digest contains non-hex characters".to_string(),
                remediation: Some("Ensure digest is lowercase hex-encoded SHA-256".to_string()),
            });
        }

        // Rule: schema version
        if manifest.schema_version == 0 {
            findings.push(LintFinding {
                rule_id: "sdk.lint.zero_schema_version".to_string(),
                severity: LintSeverity::Error,
                message: "schema_version must be >= 1".to_string(),
                remediation: Some("Set schema_version to 1".to_string()),
            });
        }

        // Rule: missing signature (info level)
        if manifest.publisher_signature.is_none() {
            findings.push(LintFinding {
                rule_id: "sdk.lint.no_signature".to_string(),
                severity: LintSeverity::Info,
                message: "manifest has no publisher signature".to_string(),
                remediation: Some(
                    "Sign the manifest for higher trust level acceptance".to_string(),
                ),
            });
        }

        // Rule: excessive capabilities
        if manifest.required_capabilities.len() > 5 {
            findings.push(LintFinding {
                rule_id: "sdk.lint.excessive_capabilities".to_string(),
                severity: LintSeverity::Warning,
                message: format!(
                    "connector requests {} capabilities (consider least-privilege)",
                    manifest.required_capabilities.len()
                ),
                remediation: Some("Remove capabilities not strictly needed".to_string()),
            });
        }

        // Rule: sensitive capability combinations
        let has_fs_write = manifest
            .required_capabilities
            .contains(&ConnectorCapability::FilesystemWrite);
        let has_net = manifest
            .required_capabilities
            .contains(&ConnectorCapability::NetworkEgress);
        let has_exec = manifest
            .required_capabilities
            .contains(&ConnectorCapability::ProcessExec);

        if has_fs_write && has_net {
            findings.push(LintFinding {
                rule_id: "sdk.lint.risky_fs_write_plus_network".to_string(),
                severity: LintSeverity::Warning,
                message: "connector requests both FilesystemWrite and NetworkEgress".to_string(),
                remediation: Some(
                    "Consider splitting into separate connectors for isolation".to_string(),
                ),
            });
        }

        if has_exec && has_net {
            findings.push(LintFinding {
                rule_id: "sdk.lint.risky_exec_plus_network".to_string(),
                severity: LintSeverity::Warning,
                message: "connector requests both ProcessExec and NetworkEgress".to_string(),
                remediation: Some(
                    "Review necessity; this combination enables remote code execution patterns"
                        .to_string(),
                ),
            });
        }

        // Policy compliance checks
        if let Some(ref policy) = self.policy {
            // Check capabilities against policy allowlist
            if !policy.max_allowed_capabilities.is_empty() {
                let allowed: HashSet<_> = policy.max_allowed_capabilities.iter().collect();
                for cap in &manifest.required_capabilities {
                    if !allowed.contains(cap) {
                        findings.push(LintFinding {
                            rule_id: "sdk.lint.capability_not_in_policy".to_string(),
                            severity: LintSeverity::Error,
                            message: format!(
                                "capability '{}' not allowed by trust policy",
                                cap.as_str()
                            ),
                            remediation: Some(
                                "Remove capability or update trust policy".to_string(),
                            ),
                        });
                    }
                }
            }

            // Check if blocked
            if policy.blocked_packages.contains(&manifest.package_id) {
                findings.push(LintFinding {
                    rule_id: "sdk.lint.package_blocked".to_string(),
                    severity: LintSeverity::Error,
                    message: "package is explicitly blocked by trust policy".to_string(),
                    remediation: Some("Remove from blocked_packages list".to_string()),
                });
            }

            // Check signature requirement
            if policy.require_signature && manifest.publisher_signature.is_none() {
                findings.push(LintFinding {
                    rule_id: "sdk.lint.missing_required_signature".to_string(),
                    severity: LintSeverity::Error,
                    message: "trust policy requires a publisher signature".to_string(),
                    remediation: Some("Sign the manifest before submission".to_string()),
                });
            }
        }

        let error_count = findings
            .iter()
            .filter(|f| f.severity == LintSeverity::Error)
            .count();
        let warning_count = findings
            .iter()
            .filter(|f| f.severity == LintSeverity::Warning)
            .count();
        let info_count = findings
            .iter()
            .filter(|f| f.severity == LintSeverity::Info)
            .count();

        let report = LintReport {
            package_id: manifest.package_id.clone(),
            findings,
            error_count,
            warning_count,
            info_count,
        };

        if self.history.len() >= LINT_HISTORY_CAPACITY {
            self.history.pop_front();
        }
        self.history.push_back(report.clone());

        report
    }

    /// Return recent lint reports.
    #[must_use]
    pub fn history(&self) -> &VecDeque<LintReport> {
        &self.history
    }

    /// Return the configured lint rules.
    #[must_use]
    pub fn rules(&self) -> &[LintRule] {
        &self.rules
    }
}

impl Default for ManifestLinter {
    fn default() -> Self {
        Self::new()
    }
}

fn default_lint_rules() -> Vec<LintRule> {
    vec![
        LintRule {
            rule_id: "sdk.lint.empty_package_id".to_string(),
            severity: LintSeverity::Error,
            description: "package_id must not be empty".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.package_id_too_long".to_string(),
            severity: LintSeverity::Error,
            description: format!("package_id exceeds {MAX_PACKAGE_ID_LEN} characters"),
        },
        LintRule {
            rule_id: "sdk.lint.package_id_invalid_chars".to_string(),
            severity: LintSeverity::Error,
            description: "package_id contains invalid characters".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.empty_version".to_string(),
            severity: LintSeverity::Error,
            description: "version must not be empty".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.non_semver_version".to_string(),
            severity: LintSeverity::Warning,
            description: "version is not semantic versioning".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.invalid_digest_length".to_string(),
            severity: LintSeverity::Error,
            description: "sha256_digest must be exactly 64 hex characters".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.invalid_digest_chars".to_string(),
            severity: LintSeverity::Error,
            description: "sha256_digest contains non-hex characters".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.zero_schema_version".to_string(),
            severity: LintSeverity::Error,
            description: "schema_version must be >= 1".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.no_signature".to_string(),
            severity: LintSeverity::Info,
            description: "manifest has no publisher signature".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.excessive_capabilities".to_string(),
            severity: LintSeverity::Warning,
            description: "connector requests more than 5 capabilities".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.risky_fs_write_plus_network".to_string(),
            severity: LintSeverity::Warning,
            description: "FilesystemWrite + NetworkEgress is a risky combination".to_string(),
        },
        LintRule {
            rule_id: "sdk.lint.risky_exec_plus_network".to_string(),
            severity: LintSeverity::Warning,
            description: "ProcessExec + NetworkEgress enables RCE patterns".to_string(),
        },
    ]
}

fn is_basic_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() >= 2
        && parts.len() <= 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

// ---------------------------------------------------------------------------
// Certification pipeline
// ---------------------------------------------------------------------------

/// Phases of the certification pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificationPhase {
    SchemaValidation,
    LintCheck,
    DigestVerification,
    CapabilityAudit,
    TrustPolicyGate,
    IntegrationProbe,
}

impl CertificationPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SchemaValidation => "schema_validation",
            Self::LintCheck => "lint_check",
            Self::DigestVerification => "digest_verification",
            Self::CapabilityAudit => "capability_audit",
            Self::TrustPolicyGate => "trust_policy_gate",
            Self::IntegrationProbe => "integration_probe",
        }
    }
}

impl std::fmt::Display for CertificationPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a single certification phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhaseVerdict {
    Passed,
    PassedWithWarnings { warnings: Vec<String> },
    Failed { reasons: Vec<String> },
    Skipped { reason: String },
}

impl PhaseVerdict {
    #[must_use]
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Passed | Self::PassedWithWarnings { .. })
    }
}

/// Result of a single certification phase execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseResult {
    pub phase: CertificationPhase,
    pub verdict: PhaseVerdict,
    pub elapsed_ms: u64,
}

/// Execution status for the integration probe stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationProbeStatus {
    Passed,
    Failed,
    Skipped,
}

/// One capability round-trip attempted during certification probing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationProbeAction {
    pub capability: ConnectorCapability,
    pub action: String,
    pub target: Option<String>,
    pub operation_id: String,
    pub decision_id: String,
}

/// Structured evidence emitted by the certification integration probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationProbeEvidence {
    pub status: IntegrationProbeStatus,
    pub host_id: String,
    pub protocol_version: ConnectorProtocolVersion,
    pub current_ft_version: String,
    pub min_ft_version: Option<String>,
    pub health_live: bool,
    pub health_ready: bool,
    pub heartbeat_recorded: bool,
    pub stopped_cleanly: bool,
    pub actions: Vec<IntegrationProbeAction>,
    pub detail: Option<String>,
}

/// Overall certification verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CertificationVerdict {
    Certified,
    ConditionalPass,
    Rejected,
}

impl CertificationVerdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Certified => "certified",
            Self::ConditionalPass => "conditional_pass",
            Self::Rejected => "rejected",
        }
    }
}

impl std::fmt::Display for CertificationVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Full certification report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificationReport {
    pub package_id: String,
    pub version: String,
    pub verdict: CertificationVerdict,
    pub phases: Vec<PhaseResult>,
    pub trust_level: Option<TrustLevel>,
    pub integration_probe: Option<IntegrationProbeEvidence>,
    pub total_elapsed_ms: u64,
}

impl CertificationReport {
    /// True if the certification passed (certified or conditional).
    #[must_use]
    pub fn passed(&self) -> bool {
        let verdict_passed = matches!(
            self.verdict,
            CertificationVerdict::Certified | CertificationVerdict::ConditionalPass
        );
        let probe_passed = self
            .integration_probe
            .as_ref()
            .is_none_or(|probe| probe.status == IntegrationProbeStatus::Passed);
        verdict_passed && probe_passed
    }
}

impl std::fmt::Display for CertificationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cert({} v{}): {} ({} phases, {}ms)",
            self.package_id,
            self.version,
            self.verdict,
            self.phases.len(),
            self.total_elapsed_ms
        )
    }
}

/// Multi-phase certification pipeline for connector packages.
pub struct CertificationPipeline {
    linter: ManifestLinter,
    policy: TrustPolicy,
    history: VecDeque<CertificationReport>,
    telemetry: CertificationTelemetry,
}

/// Telemetry for the certification pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CertificationTelemetry {
    pub total_runs: u64,
    pub certified: u64,
    pub conditional_passes: u64,
    pub rejections: u64,
    pub phase_failures: BTreeMap<String, u64>,
}

/// Snapshot of certification telemetry (for observation without mutation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificationTelemetrySnapshot {
    pub total_runs: u64,
    pub certified: u64,
    pub conditional_passes: u64,
    pub rejections: u64,
    pub phase_failures: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IntegrationProbePlan {
    host_config: ConnectorHostConfig,
    requests: Vec<ConnectorOperationRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum IntegrationProbePlanError {
    MissingPrerequisite { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IntegrationProbeExecution {
    verdict: PhaseVerdict,
    evidence: IntegrationProbeEvidence,
    warning: bool,
    failure: bool,
}

fn sanitize_probe_component(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "connector".to_string()
    } else {
        sanitized.to_string()
    }
}

fn probe_host_id(package_id: &str) -> String {
    format!("cert-probe-{}", sanitize_probe_component(package_id))
}

fn probe_zone_id(package_id: &str) -> String {
    format!("cert.probe.{}", sanitize_probe_component(package_id))
}

fn parse_semver_components(version: &str) -> Option<(u64, u64, u64)> {
    if !is_basic_semver(version) {
        return None;
    }
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(value) => value.parse().ok()?,
        None => 0,
    };
    Some((major, minor, patch))
}

fn current_ft_satisfies(min_ft_version: &str) -> Result<bool, String> {
    let current = parse_semver_components(CURRENT_FT_VERSION).ok_or_else(|| {
        format!("current certification runtime version '{CURRENT_FT_VERSION}' is not valid semver")
    })?;
    let minimum = parse_semver_components(min_ft_version)
        .ok_or_else(|| format!("connector declares invalid min_ft_version '{min_ft_version}'"))?;
    Ok(current >= minimum)
}

fn missing_probe_target_reason(
    capability: ConnectorCapability,
    metadata_key: &str,
) -> IntegrationProbePlanError {
    IntegrationProbePlanError::MissingPrerequisite {
        reason: format!(
            "integration probe requires metadata '{metadata_key}' for capability '{}'",
            capability.as_str()
        ),
    }
}

fn build_integration_probe_plan(
    manifest: &ConnectorManifest,
) -> Result<IntegrationProbePlan, IntegrationProbePlanError> {
    let mut allowed_capabilities = manifest.required_capabilities.clone();
    allowed_capabilities.sort_by_key(|capability| capability.as_str());
    allowed_capabilities.dedup();
    if allowed_capabilities.is_empty() {
        allowed_capabilities.push(ConnectorCapability::Invoke);
    }

    let mut envelope = ConnectorCapabilityEnvelope {
        allowed_capabilities,
        ..ConnectorCapabilityEnvelope::default()
    };
    let mut requests = Vec::with_capacity(manifest.required_capabilities.len());

    for (idx, capability) in manifest.required_capabilities.iter().copied().enumerate() {
        let correlation_id = format!("probe-{idx:02}");
        let request = match capability {
            ConnectorCapability::Invoke => {
                ConnectorOperationRequest::new("connector.invoke.ping", correlation_id, capability)
            }
            ConnectorCapability::ReadState => ConnectorOperationRequest::new(
                "connector.state.snapshot",
                correlation_id,
                capability,
            ),
            ConnectorCapability::StreamEvents => ConnectorOperationRequest::new(
                "connector.events.subscribe",
                correlation_id,
                capability,
            ),
            ConnectorCapability::SecretBroker => {
                ConnectorOperationRequest::new("connector.secret.fetch", correlation_id, capability)
            }
            ConnectorCapability::FilesystemRead => {
                let target = manifest
                    .metadata
                    .get(PROBE_METADATA_FILESYSTEM_READ_TARGET)
                    .cloned()
                    .ok_or_else(|| {
                        missing_probe_target_reason(
                            capability,
                            PROBE_METADATA_FILESYSTEM_READ_TARGET,
                        )
                    })?;
                envelope.filesystem_read_prefixes.push(target.clone());
                ConnectorOperationRequest::new("connector.fs.read", correlation_id, capability)
                    .with_target(target)
            }
            ConnectorCapability::FilesystemWrite => {
                let target = manifest
                    .metadata
                    .get(PROBE_METADATA_FILESYSTEM_WRITE_TARGET)
                    .cloned()
                    .ok_or_else(|| {
                        missing_probe_target_reason(
                            capability,
                            PROBE_METADATA_FILESYSTEM_WRITE_TARGET,
                        )
                    })?;
                envelope.filesystem_write_prefixes.push(target.clone());
                ConnectorOperationRequest::new("connector.fs.write", correlation_id, capability)
                    .with_target(target)
            }
            ConnectorCapability::NetworkEgress => {
                let target = manifest
                    .metadata
                    .get(PROBE_METADATA_NETWORK_TARGET)
                    .cloned()
                    .ok_or_else(|| {
                        missing_probe_target_reason(capability, PROBE_METADATA_NETWORK_TARGET)
                    })?;
                envelope.network_allow_hosts.push(target.clone());
                ConnectorOperationRequest::new(
                    "connector.network.egress",
                    correlation_id,
                    capability,
                )
                .with_target(target)
            }
            ConnectorCapability::ProcessExec => {
                let target = manifest
                    .metadata
                    .get(PROBE_METADATA_EXEC_TARGET)
                    .cloned()
                    .ok_or_else(|| {
                        missing_probe_target_reason(capability, PROBE_METADATA_EXEC_TARGET)
                    })?;
                envelope.allowed_exec_commands.push(target.clone());
                ConnectorOperationRequest::new("connector.process.exec", correlation_id, capability)
                    .with_target(target)
            }
        };
        requests.push(request);
    }

    let host_config = ConnectorHostConfig {
        host_id: probe_host_id(&manifest.package_id),
        sandbox: ConnectorSandboxZone {
            zone_id: probe_zone_id(&manifest.package_id),
            fail_closed: true,
            capability_envelope: envelope,
        },
        ..ConnectorHostConfig::default()
    };

    Ok(IntegrationProbePlan {
        host_config,
        requests,
    })
}

fn base_integration_probe_evidence(manifest: &ConnectorManifest) -> IntegrationProbeEvidence {
    IntegrationProbeEvidence {
        status: IntegrationProbeStatus::Skipped,
        host_id: probe_host_id(&manifest.package_id),
        protocol_version: ConnectorProtocolVersion::default(),
        current_ft_version: CURRENT_FT_VERSION.to_string(),
        min_ft_version: manifest.min_ft_version.clone(),
        health_live: false,
        health_ready: false,
        heartbeat_recorded: false,
        stopped_cleanly: false,
        actions: Vec::new(),
        detail: None,
    }
}

fn failed_probe_execution(
    mut evidence: IntegrationProbeEvidence,
    reason: impl Into<String>,
) -> IntegrationProbeExecution {
    let reason = reason.into();
    evidence.status = IntegrationProbeStatus::Failed;
    evidence.detail = Some(reason.clone());
    IntegrationProbeExecution {
        verdict: PhaseVerdict::Failed {
            reasons: vec![reason],
        },
        evidence,
        warning: false,
        failure: true,
    }
}

fn skipped_probe_execution(
    mut evidence: IntegrationProbeEvidence,
    reason: impl Into<String>,
) -> IntegrationProbeExecution {
    let reason = reason.into();
    evidence.status = IntegrationProbeStatus::Skipped;
    evidence.detail = Some(reason.clone());
    IntegrationProbeExecution {
        verdict: PhaseVerdict::Skipped { reason },
        evidence,
        warning: true,
        failure: false,
    }
}

fn passed_probe_execution(mut evidence: IntegrationProbeEvidence) -> IntegrationProbeExecution {
    evidence.status = IntegrationProbeStatus::Passed;
    evidence.detail = Some(if evidence.actions.is_empty() {
        "validated startup, heartbeat, and health semantics".to_string()
    } else {
        format!(
            "validated startup, heartbeat, health, and {} capability round-trip(s)",
            evidence.actions.len()
        )
    });
    IntegrationProbeExecution {
        verdict: PhaseVerdict::Passed,
        evidence,
        warning: false,
        failure: false,
    }
}

impl CertificationPipeline {
    /// Create a new certification pipeline with the given trust policy.
    #[must_use]
    pub fn new(policy: TrustPolicy) -> Self {
        Self {
            linter: ManifestLinter::with_policy(policy.clone()),
            policy,
            history: VecDeque::with_capacity(CERTIFICATION_HISTORY_CAPACITY),
            telemetry: CertificationTelemetry::default(),
        }
    }

    /// Run the full certification pipeline on a manifest + payload.
    pub fn certify(&mut self, manifest: &ConnectorManifest, payload: &[u8]) -> CertificationReport {
        let start = std::time::Instant::now();
        let mut phases = Vec::new();
        let mut any_failure = false;
        let mut any_warning = false;
        let integration_probe;

        // Phase 1: Schema validation (manifest.validate() returns ConnectorRegistryError)
        let phase_start = std::time::Instant::now();
        let schema_verdict = match manifest.validate() {
            Ok(()) => PhaseVerdict::Passed,
            Err(e) => {
                any_failure = true;
                PhaseVerdict::Failed {
                    reasons: vec![format!("{e}")],
                }
            }
        };
        phases.push(PhaseResult {
            phase: CertificationPhase::SchemaValidation,
            verdict: schema_verdict,
            elapsed_ms: phase_start.elapsed().as_millis() as u64,
        });

        // Phase 2: Lint check
        let phase_start = std::time::Instant::now();
        let lint_report = self.linter.lint(manifest);
        let lint_verdict = if lint_report.error_count > 0 {
            any_failure = true;
            PhaseVerdict::Failed {
                reasons: lint_report
                    .findings
                    .iter()
                    .filter(|f| f.severity == LintSeverity::Error)
                    .map(|f| f.message.clone())
                    .collect(),
            }
        } else if lint_report.warning_count > 0 {
            any_warning = true;
            PhaseVerdict::PassedWithWarnings {
                warnings: lint_report
                    .findings
                    .iter()
                    .filter(|f| f.severity == LintSeverity::Warning)
                    .map(|f| f.message.clone())
                    .collect(),
            }
        } else {
            PhaseVerdict::Passed
        };
        phases.push(PhaseResult {
            phase: CertificationPhase::LintCheck,
            verdict: lint_verdict,
            elapsed_ms: phase_start.elapsed().as_millis() as u64,
        });

        // Phase 3: Digest verification
        let phase_start = std::time::Instant::now();
        let computed_digest = compute_sha256_hex(payload);
        let digest_verdict = if computed_digest == manifest.sha256_digest {
            PhaseVerdict::Passed
        } else {
            any_failure = true;
            PhaseVerdict::Failed {
                reasons: vec![format!(
                    "digest mismatch: expected={}, computed={}",
                    manifest.sha256_digest, computed_digest
                )],
            }
        };
        phases.push(PhaseResult {
            phase: CertificationPhase::DigestVerification,
            verdict: digest_verdict,
            elapsed_ms: phase_start.elapsed().as_millis() as u64,
        });

        // Phase 4: Capability audit
        let phase_start = std::time::Instant::now();
        let cap_verdict = if self.policy.max_allowed_capabilities.is_empty() {
            PhaseVerdict::Passed
        } else {
            let allowed: HashSet<_> = self.policy.max_allowed_capabilities.iter().collect();
            let denied: Vec<_> = manifest
                .required_capabilities
                .iter()
                .filter(|c| !allowed.contains(c))
                .map(|c| format!("capability '{}' exceeds policy", c.as_str()))
                .collect();
            if denied.is_empty() {
                PhaseVerdict::Passed
            } else {
                any_failure = true;
                PhaseVerdict::Failed { reasons: denied }
            }
        };
        phases.push(PhaseResult {
            phase: CertificationPhase::CapabilityAudit,
            verdict: cap_verdict,
            elapsed_ms: phase_start.elapsed().as_millis() as u64,
        });

        // Phase 5: Trust policy gate
        let phase_start = std::time::Instant::now();
        let trust_level = self.policy.evaluate(manifest);
        let trust_verdict = match trust_level {
            TrustLevel::Blocked => {
                any_failure = true;
                PhaseVerdict::Failed {
                    reasons: vec!["package blocked by trust policy".to_string()],
                }
            }
            TrustLevel::Untrusted => {
                let min = &self.policy.min_install_level;
                if matches!(min, TrustLevel::Trusted) {
                    any_failure = true;
                    PhaseVerdict::Failed {
                        reasons: vec![
                            "package is untrusted; policy requires trusted level".to_string(),
                        ],
                    }
                } else {
                    any_warning = true;
                    PhaseVerdict::PassedWithWarnings {
                        warnings: vec!["package is untrusted".to_string()],
                    }
                }
            }
            TrustLevel::Conditional => {
                any_warning = true;
                PhaseVerdict::PassedWithWarnings {
                    warnings: vec!["package has conditional trust".to_string()],
                }
            }
            TrustLevel::Trusted => PhaseVerdict::Passed,
        };
        phases.push(PhaseResult {
            phase: CertificationPhase::TrustPolicyGate,
            verdict: trust_verdict,
            elapsed_ms: phase_start.elapsed().as_millis() as u64,
        });

        // Phase 6: Integration probe (sandbox lifecycle + capability round-trip)
        let phase_start = std::time::Instant::now();
        let probe_execution = if any_failure {
            skipped_probe_execution(
                base_integration_probe_evidence(manifest),
                "skipped due to prior failures",
            )
        } else {
            self.execute_integration_probe(manifest)
        };
        integration_probe = Some(probe_execution.evidence.clone());
        any_failure |= probe_execution.failure;
        any_warning |= probe_execution.warning;
        phases.push(PhaseResult {
            phase: CertificationPhase::IntegrationProbe,
            verdict: probe_execution.verdict,
            elapsed_ms: phase_start.elapsed().as_millis() as u64,
        });

        // Determine overall verdict
        let verdict = if any_failure {
            CertificationVerdict::Rejected
        } else if any_warning {
            CertificationVerdict::ConditionalPass
        } else {
            CertificationVerdict::Certified
        };

        let total_elapsed_ms = start.elapsed().as_millis() as u64;

        // Update telemetry
        self.telemetry.total_runs += 1;
        match verdict {
            CertificationVerdict::Certified => self.telemetry.certified += 1,
            CertificationVerdict::ConditionalPass => self.telemetry.conditional_passes += 1,
            CertificationVerdict::Rejected => self.telemetry.rejections += 1,
        }
        for p in &phases {
            if matches!(p.verdict, PhaseVerdict::Failed { .. }) {
                *self
                    .telemetry
                    .phase_failures
                    .entry(p.phase.as_str().to_string())
                    .or_insert(0) += 1;
            }
        }

        let report = CertificationReport {
            package_id: manifest.package_id.clone(),
            version: manifest.version.clone(),
            verdict,
            phases,
            trust_level: Some(trust_level),
            integration_probe,
            total_elapsed_ms,
        };

        if self.history.len() >= CERTIFICATION_HISTORY_CAPACITY {
            self.history.pop_front();
        }
        self.history.push_back(report.clone());

        report
    }

    /// Return recent certification reports.
    #[must_use]
    pub fn history(&self) -> &VecDeque<CertificationReport> {
        &self.history
    }

    /// Take a telemetry snapshot.
    #[must_use]
    pub fn telemetry_snapshot(&self) -> CertificationTelemetrySnapshot {
        CertificationTelemetrySnapshot {
            total_runs: self.telemetry.total_runs,
            certified: self.telemetry.certified,
            conditional_passes: self.telemetry.conditional_passes,
            rejections: self.telemetry.rejections,
            phase_failures: self.telemetry.phase_failures.clone(),
        }
    }

    fn execute_integration_probe(&self, manifest: &ConnectorManifest) -> IntegrationProbeExecution {
        let mut evidence = base_integration_probe_evidence(manifest);

        if let Some(min_ft_version) = manifest.min_ft_version.as_deref() {
            match current_ft_satisfies(min_ft_version) {
                Ok(true) => {}
                Ok(false) => {
                    return failed_probe_execution(
                        evidence,
                        format!(
                            "connector requires FrankenTerm >= {min_ft_version}, current certification runtime is {CURRENT_FT_VERSION}"
                        ),
                    );
                }
                Err(reason) => return failed_probe_execution(evidence, reason),
            }
        }

        let plan = match build_integration_probe_plan(manifest) {
            Ok(plan) => plan,
            Err(IntegrationProbePlanError::MissingPrerequisite { reason }) => {
                return skipped_probe_execution(evidence, reason);
            }
        };
        evidence.host_id = plan.host_config.host_id.clone();
        evidence.protocol_version = plan.host_config.protocol_version;

        let mut runtime = match ConnectorHostRuntime::new(plan.host_config) {
            Ok(runtime) => runtime,
            Err(err) => {
                return failed_probe_execution(
                    evidence,
                    format!("integration probe runtime creation failed: {err}"),
                );
            }
        };

        if let Err(err) = runtime.start_with_probe(1_000, StartupProbeResult::healthy()) {
            return failed_probe_execution(
                evidence,
                format!("integration probe startup failed: {err}"),
            );
        }

        if let Err(err) = runtime.record_heartbeat(1_500) {
            return failed_probe_execution(
                evidence,
                format!("integration probe heartbeat failed: {err}"),
            );
        }
        evidence.heartbeat_recorded = true;

        for (idx, request) in plan.requests.into_iter().enumerate() {
            let action = request.action.clone();
            let target = request.target.clone();
            let capability = request.capability;
            let now_ms = 2_000 + idx as u64;
            match runtime.authorize_operation(now_ms, request) {
                Ok(envelope) => evidence.actions.push(IntegrationProbeAction {
                    capability,
                    action,
                    target,
                    operation_id: envelope.operation_id,
                    decision_id: envelope.decision_id,
                }),
                Err(err) => {
                    return failed_probe_execution(
                        evidence,
                        format!(
                            "integration probe authorization failed for capability '{}': {err}",
                            capability.as_str()
                        ),
                    );
                }
            }
        }

        let health = runtime.health_snapshot(3_000);
        evidence.health_live = health.is_live;
        evidence.health_ready = health.is_ready;
        if !health.is_live || !health.is_ready {
            return failed_probe_execution(
                evidence,
                "integration probe health snapshot is not live and ready",
            );
        }

        if let Err(err) = runtime.stop(4_000) {
            return failed_probe_execution(
                evidence,
                format!("integration probe stop failed: {err}"),
            );
        }
        evidence.stopped_cleanly = runtime.state().phase() == ConnectorLifecyclePhase::Stopped;
        if !evidence.stopped_cleanly {
            return failed_probe_execution(evidence, "integration probe did not stop cleanly");
        }

        passed_probe_execution(evidence)
    }
}

// ---------------------------------------------------------------------------
// Local simulation harness
// ---------------------------------------------------------------------------

/// In-memory simulation harness for connector development.
///
/// Provides a self-contained environment to test connector lifecycle,
/// registration, and sandbox behavior without I/O.
pub struct ConnectorSimulator {
    registry: ConnectorRegistryClient,
    runtimes: HashMap<String, ConnectorHostRuntime>,
    pipeline: CertificationPipeline,
    event_log: VecDeque<SimulationEvent>,
    clock_ms: u64,
}

/// Events logged during simulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulationEvent {
    pub timestamp_ms: u64,
    pub connector_id: String,
    pub event_type: SimulationEventType,
    pub detail: String,
}

/// Types of simulation events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimulationEventType {
    Registered,
    Started,
    Stopped,
    Heartbeat,
    UsageChecked,
    OperationAuthorized,
    OperationDenied,
    CertificationRun,
    FailureRecorded,
    Restarted,
}

impl ConnectorSimulator {
    /// Create a new simulator with the given trust policy.
    #[must_use]
    pub fn new(policy: TrustPolicy) -> Self {
        let config = ConnectorRegistryConfig {
            trust_policy: policy.clone(),
            max_packages: 256,
            enforce_transparency: false,
            max_verification_history: 100,
        };
        Self {
            registry: ConnectorRegistryClient::new(config),
            runtimes: HashMap::new(),
            pipeline: CertificationPipeline::new(policy),
            event_log: VecDeque::with_capacity(1024),
            clock_ms: 0,
        }
    }

    /// Advance the simulation clock.
    pub fn tick(&mut self, delta_ms: u64) {
        self.clock_ms += delta_ms;
    }

    /// Current simulation time.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.clock_ms
    }

    /// Register and certify a connector from manifest + payload.
    pub fn register(
        &mut self,
        manifest: &ConnectorManifest,
        payload: &[u8],
        host_config: ConnectorHostConfig,
    ) -> Result<CertificationReport, ConnectorSdkError> {
        let cert_report = self.pipeline.certify(manifest, payload);

        self.log_event(
            &manifest.package_id,
            SimulationEventType::CertificationRun,
            format!("verdict={}", cert_report.verdict),
        );

        if !cert_report.passed() {
            return Ok(cert_report);
        }

        // Register in the registry
        self.registry
            .register_package(manifest.clone(), payload, self.clock_ms)
            .map_err(|e| ConnectorSdkError::RegistryError {
                reason: format!("{e}"),
            })?;

        // Create runtime
        let runtime = ConnectorHostRuntime::new(host_config).map_err(|e| {
            ConnectorSdkError::SimulationError {
                reason: format!("runtime creation failed: {e}"),
            }
        })?;
        self.runtimes.insert(manifest.package_id.clone(), runtime);

        self.log_event(
            &manifest.package_id,
            SimulationEventType::Registered,
            "package registered and runtime created".to_string(),
        );

        Ok(cert_report)
    }

    /// Start a registered connector's runtime.
    pub fn start(&mut self, connector_id: &str) -> Result<(), ConnectorSdkError> {
        let clock = self.clock_ms;
        let runtime = self.runtimes.get_mut(connector_id).ok_or_else(|| {
            ConnectorSdkError::SimulationError {
                reason: format!("connector '{connector_id}' not registered"),
            }
        })?;

        runtime
            .start(clock)
            .map_err(|e| ConnectorSdkError::SimulationError {
                reason: format!("start failed: {e}"),
            })?;

        self.log_event(
            connector_id,
            SimulationEventType::Started,
            "runtime started".to_string(),
        );

        Ok(())
    }

    /// Stop a running connector's runtime.
    pub fn stop(&mut self, connector_id: &str) -> Result<(), ConnectorSdkError> {
        let clock = self.clock_ms;
        let runtime = self.runtimes.get_mut(connector_id).ok_or_else(|| {
            ConnectorSdkError::SimulationError {
                reason: format!("connector '{connector_id}' not registered"),
            }
        })?;

        runtime
            .stop(clock)
            .map_err(|e| ConnectorSdkError::SimulationError {
                reason: format!("stop failed: {e}"),
            })?;

        self.log_event(
            connector_id,
            SimulationEventType::Stopped,
            "runtime stopped".to_string(),
        );

        Ok(())
    }

    /// Record a heartbeat for a connector.
    pub fn heartbeat(&mut self, connector_id: &str) -> Result<(), ConnectorSdkError> {
        let clock = self.clock_ms;
        let runtime = self.runtimes.get_mut(connector_id).ok_or_else(|| {
            ConnectorSdkError::SimulationError {
                reason: format!("connector '{connector_id}' not registered"),
            }
        })?;

        runtime
            .record_heartbeat(clock)
            .map_err(|e| ConnectorSdkError::SimulationError {
                reason: format!("heartbeat failed: {e}"),
            })?;

        self.log_event(
            connector_id,
            SimulationEventType::Heartbeat,
            "heartbeat recorded".to_string(),
        );

        Ok(())
    }

    /// Get the lifecycle phase of a connector.
    pub fn phase(&self, connector_id: &str) -> Result<ConnectorLifecyclePhase, ConnectorSdkError> {
        let runtime =
            self.runtimes
                .get(connector_id)
                .ok_or_else(|| ConnectorSdkError::SimulationError {
                    reason: format!("connector '{connector_id}' not registered"),
                })?;

        Ok(runtime.state().phase())
    }

    /// Return the simulation event log.
    #[must_use]
    pub fn event_log(&self) -> &VecDeque<SimulationEvent> {
        &self.event_log
    }

    /// Return the number of registered connectors.
    #[must_use]
    pub fn connector_count(&self) -> usize {
        self.runtimes.len()
    }

    /// Take a certification pipeline telemetry snapshot.
    #[must_use]
    pub fn certification_telemetry(&self) -> CertificationTelemetrySnapshot {
        self.pipeline.telemetry_snapshot()
    }

    fn log_event(&mut self, connector_id: &str, event_type: SimulationEventType, detail: String) {
        if self.event_log.len() >= 1024 {
            self.event_log.pop_front();
        }
        self.event_log.push_back(SimulationEvent {
            timestamp_ms: self.clock_ms,
            connector_id: connector_id.to_string(),
            event_type,
            detail,
        });
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Compute SHA-256 digest of bytes and return lowercase hex string.
#[must_use]
pub fn compute_sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    result
        .iter()
        .fold(String::with_capacity(result.len() * 2), |mut s, b| {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
            s
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_payload() -> Vec<u8> {
        b"hello connector world".to_vec()
    }

    fn test_digest() -> String {
        compute_sha256_hex(&test_payload())
    }

    fn minimal_manifest() -> ConnectorManifest {
        ManifestBuilder::new("test-connector")
            .version("1.0.0")
            .author("dev@example.com")
            .build_with_digest(&test_payload())
            .unwrap()
    }

    fn permissive_policy() -> TrustPolicy {
        TrustPolicyBuilder::new()
            .allow_capabilities(&[
                ConnectorCapability::Invoke,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
            ])
            .build()
    }

    fn trusted_policy(caps: &[ConnectorCapability]) -> TrustPolicy {
        TrustPolicyBuilder::new()
            .allow_capabilities(caps)
            .trusted_publisher("dev@example.com")
            .build()
    }

    fn default_host_config() -> ConnectorHostConfig {
        ConnectorHostConfig::default()
    }

    // ---- ManifestBuilder tests ----

    #[test]
    fn manifest_builder_happy_path() {
        let payload = test_payload();
        let manifest = ManifestBuilder::new("my-plugin")
            .version("2.1.0")
            .display_name("My Plugin")
            .description("A test connector")
            .author("alice@example.com")
            .capability(ConnectorCapability::Invoke)
            .capability(ConnectorCapability::ReadState)
            .build_with_digest(&payload)
            .unwrap();

        assert_eq!(manifest.package_id, "my-plugin");
        assert_eq!(manifest.version, "2.1.0");
        assert_eq!(manifest.display_name, "My Plugin");
        assert_eq!(manifest.author, "alice@example.com");
        assert_eq!(manifest.required_capabilities.len(), 2);
        assert_eq!(manifest.sha256_digest.len(), 64);
        assert!(
            manifest
                .sha256_digest
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn manifest_builder_missing_version() {
        let err = ManifestBuilder::new("test")
            .build_with_digest(&test_payload())
            .unwrap_err();
        assert!(matches!(err, ConnectorSdkError::ManifestBuilder { .. }));
    }

    #[test]
    fn manifest_builder_empty_package_id() {
        let err = ManifestBuilder::new("")
            .version("1.0.0")
            .build_with_digest(&test_payload())
            .unwrap_err();
        assert!(matches!(err, ConnectorSdkError::ManifestBuilder { .. }));
    }

    #[test]
    fn manifest_builder_dedup_capabilities() {
        let manifest = ManifestBuilder::new("test")
            .version("1.0.0")
            .capability(ConnectorCapability::Invoke)
            .capability(ConnectorCapability::Invoke)
            .capability(ConnectorCapability::ReadState)
            .build_with_digest(&test_payload())
            .unwrap();

        assert_eq!(manifest.required_capabilities.len(), 2);
    }

    #[test]
    fn manifest_builder_precomputed_digest() {
        let digest = test_digest();
        let manifest = ManifestBuilder::new("test")
            .version("1.0.0")
            .build_with_precomputed_digest(&digest)
            .unwrap();

        assert_eq!(manifest.sha256_digest, digest);
    }

    #[test]
    fn manifest_builder_defaults_display_name_to_id() {
        let manifest = ManifestBuilder::new("my-connector")
            .version("1.0.0")
            .build_with_digest(&test_payload())
            .unwrap();

        assert_eq!(manifest.display_name, "my-connector");
    }

    // ---- TrustPolicyBuilder tests ----

    #[test]
    fn trust_policy_builder_default() {
        let policy = TrustPolicyBuilder::new().build();
        assert_eq!(policy.min_install_level, TrustLevel::Conditional);
        assert!(!policy.require_signature);
        assert!(!policy.require_transparency_proof);
    }

    #[test]
    fn trust_policy_builder_strict() {
        let policy = TrustPolicyBuilder::strict().build();
        assert_eq!(policy.min_install_level, TrustLevel::Trusted);
        assert!(policy.require_signature);
        assert!(policy.require_transparency_proof);
    }

    #[test]
    fn trust_policy_builder_capabilities() {
        let policy = TrustPolicyBuilder::new()
            .allow_capability(ConnectorCapability::Invoke)
            .allow_capability(ConnectorCapability::Invoke)
            .allow_capability(ConnectorCapability::ReadState)
            .build();

        assert_eq!(policy.max_allowed_capabilities.len(), 2);
    }

    #[test]
    fn trust_policy_builder_trusted_transparency_root() {
        let policy = TrustPolicyBuilder::new()
            .trusted_transparency_root("abcd1234")
            .build();

        assert_eq!(policy.trusted_transparency_roots, vec!["abcd1234"]);
    }

    #[test]
    fn trust_policy_builder_blocked() {
        let policy = TrustPolicyBuilder::new()
            .block_package("bad-connector")
            .trusted_publisher("alice@example.com")
            .build();

        assert_eq!(policy.blocked_packages, vec!["bad-connector"]);
        assert_eq!(policy.trusted_publishers, vec!["alice@example.com"]);
    }

    // ---- SandboxBuilder tests ----

    #[test]
    fn sandbox_builder_default() {
        let sandbox = SandboxBuilder::new("zone.test").build();
        assert_eq!(sandbox.zone_id, "zone.test");
        assert!(sandbox.fail_closed);
        assert!(sandbox.capability_envelope.allowed_capabilities.is_empty());
    }

    #[test]
    fn sandbox_builder_with_capabilities() {
        let sandbox = SandboxBuilder::new("zone.dev")
            .allow(ConnectorCapability::Invoke)
            .allow(ConnectorCapability::FilesystemRead)
            .read_path("/tmp/connector")
            .network_host("api.example.com")
            .build();

        assert_eq!(sandbox.capability_envelope.allowed_capabilities.len(), 2);
        assert_eq!(
            sandbox.capability_envelope.filesystem_read_prefixes,
            vec!["/tmp/connector"]
        );
        assert_eq!(
            sandbox.capability_envelope.network_allow_hosts,
            vec!["api.example.com"]
        );
    }

    #[test]
    fn sandbox_builder_fail_open() {
        let sandbox = SandboxBuilder::new("zone.permissive").fail_open().build();
        assert!(!sandbox.fail_closed);
    }

    // ---- ManifestLinter tests ----

    #[test]
    fn lint_clean_manifest() {
        let mut linter = ManifestLinter::new();
        let manifest = minimal_manifest();
        let report = linter.lint(&manifest);

        assert!(report.passed());
        assert_eq!(report.error_count, 0);
        // Should have info about missing signature
        assert!(report.info_count > 0);
    }

    #[test]
    fn lint_empty_package_id() {
        let mut linter = ManifestLinter::new();
        let manifest = ConnectorManifest {
            schema_version: 1,
            package_id: String::new(),
            version: "1.0.0".to_string(),
            display_name: "Test".to_string(),
            description: String::new(),
            author: String::new(),
            min_ft_version: None,
            sha256_digest: test_digest(),
            required_capabilities: vec![],
            publisher_signature: None,
            transparency_token: None,
            created_at_ms: 0,
            metadata: std::collections::BTreeMap::new(),
        };
        let report = linter.lint(&manifest);
        assert!(!report.passed());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.empty_package_id")
        );
    }

    #[test]
    fn lint_invalid_digest() {
        let mut linter = ManifestLinter::new();
        let manifest = ManifestBuilder::new("test")
            .version("1.0.0")
            .build_with_precomputed_digest("too-short")
            .unwrap();
        let report = linter.lint(&manifest);
        assert!(!report.passed());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.invalid_digest_length")
        );
    }

    #[test]
    fn lint_non_semver_version() {
        let mut linter = ManifestLinter::new();
        let manifest = ManifestBuilder::new("test")
            .version("latest")
            .build_with_digest(&test_payload())
            .unwrap();
        let report = linter.lint(&manifest);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.non_semver_version")
        );
    }

    #[test]
    fn lint_excessive_capabilities() {
        let mut linter = ManifestLinter::new();
        let manifest = ManifestBuilder::new("test")
            .version("1.0.0")
            .capabilities(&[
                ConnectorCapability::Invoke,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
                ConnectorCapability::FilesystemRead,
                ConnectorCapability::FilesystemWrite,
                ConnectorCapability::NetworkEgress,
            ])
            .build_with_digest(&test_payload())
            .unwrap();
        let report = linter.lint(&manifest);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.excessive_capabilities")
        );
    }

    #[test]
    fn lint_risky_capability_combo() {
        let mut linter = ManifestLinter::new();
        let manifest = ManifestBuilder::new("test")
            .version("1.0.0")
            .capability(ConnectorCapability::FilesystemWrite)
            .capability(ConnectorCapability::NetworkEgress)
            .build_with_digest(&test_payload())
            .unwrap();
        let report = linter.lint(&manifest);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.risky_fs_write_plus_network")
        );
    }

    #[test]
    fn lint_policy_capability_violation() {
        let policy = TrustPolicyBuilder::new()
            .allow_capability(ConnectorCapability::Invoke)
            .build();
        let mut linter = ManifestLinter::with_policy(policy);
        let manifest = ManifestBuilder::new("test")
            .version("1.0.0")
            .capability(ConnectorCapability::Invoke)
            .capability(ConnectorCapability::ProcessExec)
            .build_with_digest(&test_payload())
            .unwrap();
        let report = linter.lint(&manifest);
        assert!(!report.passed());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.capability_not_in_policy")
        );
    }

    #[test]
    fn lint_policy_signature_required() {
        let policy = TrustPolicyBuilder::strict().build();
        let mut linter = ManifestLinter::with_policy(policy);
        let manifest = minimal_manifest();
        let report = linter.lint(&manifest);
        assert!(!report.passed());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.missing_required_signature")
        );
    }

    #[test]
    fn lint_history_preserved() {
        let mut linter = ManifestLinter::new();
        let m1 = minimal_manifest();
        linter.lint(&m1);
        linter.lint(&m1);
        assert_eq!(linter.history().len(), 2);
    }

    #[test]
    fn lint_invalid_package_id_chars() {
        let mut linter = ManifestLinter::new();
        let manifest = ManifestBuilder::new("bad name/here")
            .version("1.0.0")
            .build_with_digest(&test_payload())
            .unwrap();
        let report = linter.lint(&manifest);
        assert!(!report.passed());
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.rule_id == "sdk.lint.package_id_invalid_chars")
        );
    }

    // ---- CertificationPipeline tests ----

    #[test]
    fn certification_happy_path() {
        let policy = permissive_policy();
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("good-connector")
            .version("1.0.0")
            .author("dev@example.com")
            .capability(ConnectorCapability::Invoke)
            .build_with_digest(&payload)
            .unwrap();

        let report = pipeline.certify(&manifest, &payload);
        assert!(report.passed());
        assert_eq!(report.phases.len(), 6);
        assert!(report.trust_level.is_some());
        let probe = report.integration_probe.as_ref().unwrap();
        assert_eq!(probe.status, IntegrationProbeStatus::Passed);
        assert_eq!(probe.actions.len(), 1);
    }

    #[test]
    fn certification_digest_mismatch() {
        let policy = permissive_policy();
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("bad-digest")
            .version("1.0.0")
            .build_with_precomputed_digest("x".repeat(64))
            .unwrap();

        let report = pipeline.certify(&manifest, &payload);
        assert!(!report.passed());
        assert_eq!(report.verdict, CertificationVerdict::Rejected);
        assert!(
            report
                .phases
                .iter()
                .any(|p| p.phase == CertificationPhase::DigestVerification && !p.verdict.is_pass())
        );
    }

    #[test]
    fn certification_capability_denied() {
        let policy = TrustPolicyBuilder::new()
            .allow_capability(ConnectorCapability::Invoke)
            .build();
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("cap-denied")
            .version("1.0.0")
            .capability(ConnectorCapability::ProcessExec)
            .build_with_digest(&payload)
            .unwrap();

        let report = pipeline.certify(&manifest, &payload);
        assert!(!report.passed());
    }

    #[test]
    fn certification_telemetry_counts() {
        let policy = permissive_policy();
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let good_manifest = ManifestBuilder::new("good")
            .version("1.0.0")
            .build_with_digest(&payload)
            .unwrap();
        let bad_manifest = ManifestBuilder::new("bad")
            .version("1.0.0")
            .build_with_precomputed_digest("x".repeat(64))
            .unwrap();

        pipeline.certify(&good_manifest, &payload);
        pipeline.certify(&bad_manifest, &payload);

        let telem = pipeline.telemetry_snapshot();
        assert_eq!(telem.total_runs, 2);
        assert!(telem.rejections >= 1);
    }

    #[test]
    fn certification_integration_probe_skipped_on_failure() {
        let policy = permissive_policy();
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("fail-early")
            .version("1.0.0")
            .build_with_precomputed_digest("a".repeat(64))
            .unwrap();

        let report = pipeline.certify(&manifest, &payload);
        let probe = report
            .phases
            .iter()
            .find(|p| p.phase == CertificationPhase::IntegrationProbe)
            .unwrap();
        assert!(matches!(probe.verdict, PhaseVerdict::Skipped { .. }));
        assert_eq!(
            report.integration_probe.as_ref().unwrap().status,
            IntegrationProbeStatus::Skipped
        );
    }

    #[test]
    fn certification_integration_probe_skips_missing_target_prerequisite() {
        let policy = trusted_policy(&[ConnectorCapability::FilesystemRead]);
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("fs-read-probe")
            .version("1.0.0")
            .author("dev@example.com")
            .publisher_signature("test-sig")
            .capability(ConnectorCapability::FilesystemRead)
            .build_with_digest(&payload)
            .unwrap();

        let report = pipeline.certify(&manifest, &payload);
        assert_eq!(report.verdict, CertificationVerdict::ConditionalPass);
        assert!(!report.passed());
        let probe = report.integration_probe.as_ref().unwrap();
        assert_eq!(probe.status, IntegrationProbeStatus::Skipped);
        assert!(
            probe
                .detail
                .as_deref()
                .unwrap()
                .contains(PROBE_METADATA_FILESYSTEM_READ_TARGET)
        );
    }

    #[test]
    fn certification_integration_probe_fails_invalid_target_contract() {
        let policy = trusted_policy(&[ConnectorCapability::FilesystemRead]);
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let mut manifest = ManifestBuilder::new("fs-read-bad-target")
            .version("1.0.0")
            .author("dev@example.com")
            .publisher_signature("test-sig")
            .capability(ConnectorCapability::FilesystemRead)
            .build_with_digest(&payload)
            .unwrap();
        manifest.metadata.insert(
            PROBE_METADATA_FILESYSTEM_READ_TARGET.to_string(),
            "relative/path.txt".to_string(),
        );

        let report = pipeline.certify(&manifest, &payload);
        assert_eq!(report.verdict, CertificationVerdict::Rejected);
        assert!(!report.passed());
        let probe = report.integration_probe.as_ref().unwrap();
        assert_eq!(probe.status, IntegrationProbeStatus::Failed);
        assert!(
            probe
                .detail
                .as_deref()
                .unwrap()
                .contains("authorization failed")
        );
    }

    #[test]
    fn certification_integration_probe_rejects_incompatible_min_ft_version() {
        let policy = trusted_policy(&[ConnectorCapability::Invoke]);
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("future-connector")
            .version("1.0.0")
            .author("dev@example.com")
            .publisher_signature("test-sig")
            .min_ft_version("999.0.0")
            .capability(ConnectorCapability::Invoke)
            .build_with_digest(&payload)
            .unwrap();

        let report = pipeline.certify(&manifest, &payload);
        assert_eq!(report.verdict, CertificationVerdict::Rejected);
        assert!(!report.passed());
        let probe = report.integration_probe.as_ref().unwrap();
        assert_eq!(probe.status, IntegrationProbeStatus::Failed);
        assert!(
            probe
                .detail
                .as_deref()
                .unwrap()
                .contains("current certification runtime")
        );
    }

    #[test]
    fn certification_history_preserved() {
        let policy = permissive_policy();
        let mut pipeline = CertificationPipeline::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("hist")
            .version("1.0.0")
            .build_with_digest(&payload)
            .unwrap();

        pipeline.certify(&manifest, &payload);
        pipeline.certify(&manifest, &payload);
        assert_eq!(pipeline.history().len(), 2);
    }

    // ---- ConnectorSimulator tests ----

    #[test]
    fn simulator_register_and_start() {
        let policy = permissive_policy();
        let mut sim = ConnectorSimulator::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("sim-connector")
            .version("1.0.0")
            .publisher_signature("test-sig")
            .build_with_digest(&payload)
            .unwrap();

        let report = sim
            .register(&manifest, &payload, default_host_config())
            .unwrap();
        assert!(report.passed());
        assert_eq!(sim.connector_count(), 1);

        sim.tick(100);
        sim.start("sim-connector").unwrap();
        assert_eq!(
            sim.phase("sim-connector").unwrap(),
            ConnectorLifecyclePhase::Running
        );
    }

    #[test]
    fn simulator_start_stop_cycle() {
        let policy = permissive_policy();
        let mut sim = ConnectorSimulator::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("lifecycle")
            .version("1.0.0")
            .publisher_signature("test-sig")
            .build_with_digest(&payload)
            .unwrap();

        sim.register(&manifest, &payload, default_host_config())
            .unwrap();
        sim.tick(50);
        sim.start("lifecycle").unwrap();
        assert_eq!(
            sim.phase("lifecycle").unwrap(),
            ConnectorLifecyclePhase::Running
        );

        sim.tick(1000);
        sim.heartbeat("lifecycle").unwrap();

        sim.tick(500);
        sim.stop("lifecycle").unwrap();
        assert_eq!(
            sim.phase("lifecycle").unwrap(),
            ConnectorLifecyclePhase::Stopped
        );
    }

    #[test]
    fn simulator_certification_failure_skips_registration() {
        let policy = TrustPolicyBuilder::new()
            .allow_capability(ConnectorCapability::Invoke)
            .build();
        let mut sim = ConnectorSimulator::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("denied")
            .version("1.0.0")
            .capability(ConnectorCapability::ProcessExec)
            .build_with_digest(&payload)
            .unwrap();

        let report = sim
            .register(&manifest, &payload, default_host_config())
            .unwrap();
        assert!(!report.passed());
        assert_eq!(sim.connector_count(), 0);
    }

    #[test]
    fn simulator_probe_skip_does_not_register_connector() {
        let policy = trusted_policy(&[ConnectorCapability::FilesystemRead]);
        let mut sim = ConnectorSimulator::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("needs-fs-target")
            .version("1.0.0")
            .author("dev@example.com")
            .publisher_signature("test-sig")
            .capability(ConnectorCapability::FilesystemRead)
            .build_with_digest(&payload)
            .unwrap();

        let report = sim
            .register(&manifest, &payload, default_host_config())
            .unwrap();
        assert_eq!(report.verdict, CertificationVerdict::ConditionalPass);
        assert!(!report.passed());
        assert_eq!(sim.connector_count(), 0);
    }

    #[test]
    fn simulator_event_log() {
        let policy = permissive_policy();
        let mut sim = ConnectorSimulator::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("logged")
            .version("1.0.0")
            .publisher_signature("test-sig")
            .build_with_digest(&payload)
            .unwrap();

        sim.register(&manifest, &payload, default_host_config())
            .unwrap();
        sim.start("logged").unwrap();
        sim.heartbeat("logged").unwrap();

        let events = sim.event_log();
        assert!(events.len() >= 3);
        assert!(
            events
                .iter()
                .any(|e| e.event_type == SimulationEventType::Registered)
        );
        assert!(
            events
                .iter()
                .any(|e| e.event_type == SimulationEventType::Started)
        );
        assert!(
            events
                .iter()
                .any(|e| e.event_type == SimulationEventType::Heartbeat)
        );
    }

    #[test]
    fn simulator_unknown_connector_error() {
        let policy = permissive_policy();
        let mut sim = ConnectorSimulator::new(policy);
        let err = sim.start("nonexistent").unwrap_err();
        assert!(matches!(err, ConnectorSdkError::SimulationError { .. }));
    }

    #[test]
    fn simulator_certification_telemetry() {
        let policy = permissive_policy();
        let mut sim = ConnectorSimulator::new(policy);
        let payload = test_payload();
        let manifest = ManifestBuilder::new("tel")
            .version("1.0.0")
            .publisher_signature("test-sig")
            .build_with_digest(&payload)
            .unwrap();

        sim.register(&manifest, &payload, default_host_config())
            .unwrap();

        let telem = sim.certification_telemetry();
        assert_eq!(telem.total_runs, 1);
    }

    // ---- Utility tests ----

    #[test]
    fn sha256_deterministic() {
        let d1 = compute_sha256_hex(b"hello");
        let d2 = compute_sha256_hex(b"hello");
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
    }

    #[test]
    fn sha256_different_inputs() {
        let d1 = compute_sha256_hex(b"hello");
        let d2 = compute_sha256_hex(b"world");
        assert_ne!(d1, d2);
    }

    #[test]
    fn is_semver_valid() {
        assert!(is_basic_semver("1.0.0"));
        assert!(is_basic_semver("2.1"));
        assert!(is_basic_semver("10.20.30"));
    }

    #[test]
    fn is_semver_invalid() {
        assert!(!is_basic_semver(""));
        assert!(!is_basic_semver("latest"));
        assert!(!is_basic_semver("1.0.0.0"));
        assert!(!is_basic_semver("v1.0"));
    }

    // ---- Serde roundtrip tests ----

    #[test]
    fn lint_finding_serde_roundtrip() {
        let finding = LintFinding {
            rule_id: "sdk.lint.test".to_string(),
            severity: LintSeverity::Warning,
            message: "test finding".to_string(),
            remediation: Some("fix it".to_string()),
        };
        let json = serde_json::to_string(&finding).unwrap();
        let recovered: LintFinding = serde_json::from_str(&json).unwrap();
        assert_eq!(finding, recovered);
    }

    #[test]
    fn certification_report_serde_roundtrip() {
        let report = CertificationReport {
            package_id: "test".to_string(),
            version: "1.0.0".to_string(),
            verdict: CertificationVerdict::Certified,
            phases: vec![PhaseResult {
                phase: CertificationPhase::SchemaValidation,
                verdict: PhaseVerdict::Passed,
                elapsed_ms: 0,
            }],
            trust_level: Some(TrustLevel::Trusted),
            integration_probe: None,
            total_elapsed_ms: 1,
        };
        let json = serde_json::to_string(&report).unwrap();
        let recovered: CertificationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, recovered);
    }

    #[test]
    fn simulation_event_serde_roundtrip() {
        let event = SimulationEvent {
            timestamp_ms: 42,
            connector_id: "test".to_string(),
            event_type: SimulationEventType::Started,
            detail: "hello".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let recovered: SimulationEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, recovered);
    }

    // ---- Display tests ----

    #[test]
    fn lint_severity_display() {
        assert_eq!(format!("{}", LintSeverity::Error), "error");
        assert_eq!(format!("{}", LintSeverity::Warning), "warning");
        assert_eq!(format!("{}", LintSeverity::Info), "info");
    }

    #[test]
    fn certification_verdict_display() {
        assert_eq!(format!("{}", CertificationVerdict::Certified), "certified");
        assert_eq!(
            format!("{}", CertificationVerdict::ConditionalPass),
            "conditional_pass"
        );
        assert_eq!(format!("{}", CertificationVerdict::Rejected), "rejected");
    }

    #[test]
    fn certification_phase_display() {
        assert_eq!(
            format!("{}", CertificationPhase::DigestVerification),
            "digest_verification"
        );
    }

    #[test]
    fn lint_report_display() {
        let report = LintReport {
            package_id: "test".to_string(),
            findings: vec![],
            error_count: 1,
            warning_count: 2,
            info_count: 3,
        };
        let s = format!("{report}");
        assert!(s.contains("1 error(s)"));
        assert!(s.contains("2 warning(s)"));
    }

    #[test]
    fn lint_finding_display() {
        let finding = LintFinding {
            rule_id: "sdk.lint.test".to_string(),
            severity: LintSeverity::Error,
            message: "bad thing".to_string(),
            remediation: Some("fix it".to_string()),
        };
        let s = format!("{finding}");
        assert!(s.contains("error"));
        assert!(s.contains("bad thing"));
        assert!(s.contains("fix: fix it"));
    }

    #[test]
    fn certification_report_display() {
        let report = CertificationReport {
            package_id: "test".to_string(),
            version: "1.0.0".to_string(),
            verdict: CertificationVerdict::Certified,
            phases: vec![],
            trust_level: None,
            integration_probe: None,
            total_elapsed_ms: 42,
        };
        let s = format!("{report}");
        assert!(s.contains("certified"));
        assert!(s.contains("42ms"));
    }
}
