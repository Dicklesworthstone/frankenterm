//! Tier-1 connector bundle definitions and audit-chain ingestion pipeline.
//!
//! Provides a connector bundle registry for declaring, validating, and managing
//! sets of connectors (e.g. GitHub, Slack, Linear, Discord) as cohesive units.
//! Each bundle carries manifest snapshots, capability requirements, and
//! normalized audit-chain ingestion for compliance and forensic traceability.
//!
//! Part of ft-3681t.5.9.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::connector_event_model::{CanonicalConnectorEvent, EventDirection};
use crate::connector_host_runtime::{ConnectorCapability, ConnectorLifecyclePhase};
use crate::connector_registry::{ConnectorManifest, TrustLevel};
use crate::policy_audit_chain::{AuditChain, AuditEntryKind};

// =============================================================================
// Bundle tier classification
// =============================================================================

/// Tier classification for connector bundles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleTier {
    /// Tier 1 — core, high-value connectors (GitHub, Slack, Linear).
    Tier1,
    /// Tier 2 — extended connectors (Notion, Gmail, Jira).
    Tier2,
    /// Tier 3 — community/experimental connectors.
    Tier3,
    /// Custom — user-authored or enterprise-specific.
    Custom,
}

impl std::fmt::Display for BundleTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tier1 => f.write_str("tier1"),
            Self::Tier2 => f.write_str("tier2"),
            Self::Tier3 => f.write_str("tier3"),
            Self::Custom => f.write_str("custom"),
        }
    }
}

impl BundleTier {
    /// Minimum trust level required to install connectors of this tier.
    #[must_use]
    pub fn minimum_trust(&self) -> TrustLevel {
        match self {
            Self::Tier1 | Self::Tier2 => TrustLevel::Trusted,
            Self::Tier3 => TrustLevel::Conditional,
            Self::Custom => TrustLevel::Untrusted,
        }
    }

    /// Whether this tier requires signed manifests.
    #[must_use]
    pub fn requires_signed_manifest(&self) -> bool {
        matches!(self, Self::Tier1 | Self::Tier2)
    }
}

// =============================================================================
// Bundle category
// =============================================================================

/// Functional category for connector bundles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleCategory {
    /// Source code management (GitHub, GitLab, Bitbucket).
    SourceControl,
    /// Messaging and chat (Slack, Discord, Teams).
    Messaging,
    /// Project management (Linear, Jira, Asana).
    ProjectManagement,
    /// Knowledge and documentation (Notion, Confluence).
    Knowledge,
    /// CI/CD and automation (GitHub Actions, CircleCI).
    CiCd,
    /// Email and communication (Gmail, Outlook).
    Email,
    /// Monitoring and observability (Datadog, PagerDuty).
    Monitoring,
    /// General purpose / uncategorized.
    General,
}

impl std::fmt::Display for BundleCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceControl => f.write_str("source_control"),
            Self::Messaging => f.write_str("messaging"),
            Self::ProjectManagement => f.write_str("project_management"),
            Self::Knowledge => f.write_str("knowledge"),
            Self::CiCd => f.write_str("ci_cd"),
            Self::Email => f.write_str("email"),
            Self::Monitoring => f.write_str("monitoring"),
            Self::General => f.write_str("general"),
        }
    }
}

// =============================================================================
// Connector entry within a bundle
// =============================================================================

/// A single connector entry within a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleConnectorEntry {
    /// Connector package ID (matches ConnectorManifest.package_id).
    pub package_id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Minimum required version.
    pub min_version: String,
    /// Whether this connector is required for the bundle to be valid.
    pub required: bool,
    /// Capabilities this connector must provide.
    pub required_capabilities: Vec<ConnectorCapability>,
    /// Snapshot of the connector manifest at bundle creation time.
    pub manifest_snapshot: Option<ConnectorManifest>,
    /// Custom metadata for this entry.
    pub metadata: BTreeMap<String, String>,
}

impl BundleConnectorEntry {
    /// Create a required connector entry.
    #[must_use]
    pub fn required(package_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            package_id: package_id.into(),
            display_name: display_name.into(),
            min_version: String::new(),
            required: true,
            required_capabilities: Vec::new(),
            manifest_snapshot: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Create an optional connector entry.
    #[must_use]
    pub fn optional(package_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            package_id: package_id.into(),
            display_name: display_name.into(),
            min_version: String::new(),
            required: false,
            required_capabilities: Vec::new(),
            manifest_snapshot: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Set the minimum required version.
    #[must_use]
    pub fn with_min_version(mut self, version: impl Into<String>) -> Self {
        self.min_version = version.into();
        self
    }

    /// Add a required capability.
    #[must_use]
    pub fn with_capability(mut self, cap: ConnectorCapability) -> Self {
        if !self.required_capabilities.contains(&cap) {
            self.required_capabilities.push(cap);
        }
        self
    }

    /// Attach a manifest snapshot.
    #[must_use]
    pub fn with_manifest(mut self, manifest: ConnectorManifest) -> Self {
        self.manifest_snapshot = Some(manifest);
        self
    }
}

// =============================================================================
// Connector bundle
// =============================================================================

/// A connector bundle — a named, versioned set of related connectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorBundle {
    /// Unique bundle identifier (e.g. "ft-bundle-devtools").
    pub bundle_id: String,
    /// Human-readable name.
    pub display_name: String,
    /// Bundle description.
    pub description: String,
    /// Bundle version.
    pub version: String,
    /// Tier classification.
    pub tier: BundleTier,
    /// Functional category.
    pub category: BundleCategory,
    /// Author / publisher.
    pub author: String,
    /// Minimum FrankenTerm version required.
    pub min_ft_version: Option<String>,
    /// Connector entries in this bundle.
    pub connectors: Vec<BundleConnectorEntry>,
    /// Labels for filtering and search.
    pub labels: BTreeSet<String>,
    /// When the bundle was created (epoch ms).
    pub created_at_ms: u64,
    /// When the bundle was last updated (epoch ms).
    pub updated_at_ms: u64,
    /// Custom metadata.
    pub metadata: BTreeMap<String, String>,
}

impl ConnectorBundle {
    /// Create a new bundle.
    #[must_use]
    pub fn new(
        bundle_id: impl Into<String>,
        display_name: impl Into<String>,
        tier: BundleTier,
        category: BundleCategory,
        now_ms: u64,
    ) -> Self {
        Self {
            bundle_id: bundle_id.into(),
            display_name: display_name.into(),
            description: String::new(),
            version: "0.1.0".to_string(),
            tier,
            category,
            author: String::new(),
            min_ft_version: None,
            connectors: Vec::new(),
            labels: BTreeSet::new(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            metadata: BTreeMap::new(),
        }
    }

    /// Set the description.
    #[must_use]
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Set the version.
    #[must_use]
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Set the author.
    #[must_use]
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = author.into();
        self
    }

    /// Add a connector to the bundle.
    pub fn add_connector(&mut self, entry: BundleConnectorEntry) {
        self.connectors.push(entry);
    }

    /// Add a label.
    pub fn add_label(&mut self, label: impl Into<String>) {
        self.labels.insert(label.into());
    }

    /// Number of connectors in this bundle.
    #[must_use]
    pub fn connector_count(&self) -> usize {
        self.connectors.len()
    }

    /// Number of required connectors.
    #[must_use]
    pub fn required_count(&self) -> usize {
        self.connectors.iter().filter(|c| c.required).count()
    }

    /// Number of optional connectors.
    #[must_use]
    pub fn optional_count(&self) -> usize {
        self.connectors.iter().filter(|c| !c.required).count()
    }

    /// All distinct package IDs.
    #[must_use]
    pub fn package_ids(&self) -> Vec<&str> {
        self.connectors
            .iter()
            .map(|c| c.package_id.as_str())
            .collect()
    }

    /// Union of all required capabilities across connectors.
    #[must_use]
    pub fn all_required_capabilities(&self) -> Vec<ConnectorCapability> {
        let mut caps: BTreeSet<ConnectorCapability> = BTreeSet::new();
        for entry in &self.connectors {
            for cap in &entry.required_capabilities {
                caps.insert(*cap);
            }
        }
        caps.into_iter().collect()
    }

    /// Whether all required connectors have manifest snapshots.
    #[must_use]
    pub fn all_required_have_manifests(&self) -> bool {
        self.connectors
            .iter()
            .filter(|c| c.required)
            .all(|c| c.manifest_snapshot.is_some())
    }
}

// =============================================================================
// Bundle validation
// =============================================================================

/// Outcome of bundle validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleValidationResult {
    /// Whether the bundle is valid.
    pub valid: bool,
    /// Validation errors.
    pub errors: Vec<String>,
    /// Validation warnings.
    pub warnings: Vec<String>,
}

impl BundleValidationResult {
    /// No issues.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }

    /// Accumulate an error.
    pub fn error(&mut self, msg: impl Into<String>) {
        self.valid = false;
        self.errors.push(msg.into());
    }

    /// Accumulate a warning.
    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

impl std::fmt::Display for BundleValidationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.valid {
            write!(f, "valid")?;
        } else {
            write!(f, "INVALID ({} errors)", self.errors.len())?;
        }
        if !self.warnings.is_empty() {
            write!(f, " ({} warnings)", self.warnings.len())?;
        }
        Ok(())
    }
}

/// Validate a connector bundle.
#[must_use]
pub fn validate_bundle(bundle: &ConnectorBundle) -> BundleValidationResult {
    let mut result = BundleValidationResult::ok();

    if bundle.bundle_id.is_empty() {
        result.error("bundle_id is empty");
    }
    if bundle.display_name.is_empty() {
        result.error("display_name is empty");
    }
    if bundle.version.is_empty() {
        result.error("version is empty");
    }
    if bundle.connectors.is_empty() {
        result.error("bundle has no connectors");
    }

    // Check for duplicate package IDs.
    let mut seen = BTreeSet::new();
    for entry in &bundle.connectors {
        if entry.package_id.is_empty() {
            result.error("connector entry has empty package_id");
        }
        if !seen.insert(&entry.package_id) {
            result.error(format!(
                "duplicate package_id in bundle: {}",
                entry.package_id
            ));
        }
    }

    // Tier-specific checks.
    if bundle.tier.requires_signed_manifest() {
        for entry in bundle.connectors.iter().filter(|c| c.required) {
            if let Some(ref manifest) = entry.manifest_snapshot {
                if manifest.publisher_signature.is_none() {
                    result.warn(format!(
                        "required connector '{}' in tier {} bundle lacks signed manifest",
                        entry.package_id, bundle.tier
                    ));
                }
            } else {
                result.warn(format!(
                    "required connector '{}' has no manifest snapshot",
                    entry.package_id
                ));
            }
        }
    }

    if bundle.author.is_empty() {
        result.warn("bundle author is empty");
    }

    result
}

// =============================================================================
// Audit-chain ingestion
// =============================================================================

/// Outcome of ingesting an event into the audit chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestionOutcome {
    /// Event was recorded in the audit chain.
    Recorded,
    /// Event was filtered out (not relevant for audit).
    Filtered,
    /// Event was rejected (validation failure).
    Rejected { reason: String },
}

impl std::fmt::Display for IngestionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recorded => f.write_str("recorded"),
            Self::Filtered => f.write_str("filtered"),
            Self::Rejected { reason } => write!(f, "rejected: {reason}"),
        }
    }
}

/// Configuration for the audit-chain ingestion pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestionPipelineConfig {
    /// Maximum events ingested per second (0 = unlimited).
    pub max_ingest_per_sec: u64,
    /// Whether to record lifecycle events.
    pub ingest_lifecycle: bool,
    /// Whether to record inbound signal events.
    pub ingest_inbound: bool,
    /// Whether to record outbound action events.
    pub ingest_outbound: bool,
    /// Minimum severity to ingest (events below this are filtered).
    pub min_severity_level: u32,
    /// Maximum audit trail entries to retain.
    pub max_audit_entries: usize,
}

impl Default for IngestionPipelineConfig {
    fn default() -> Self {
        Self {
            max_ingest_per_sec: 0,
            ingest_lifecycle: true,
            ingest_inbound: true,
            ingest_outbound: true,
            min_severity_level: 0,
            max_audit_entries: 4096,
        }
    }
}

/// Telemetry counters for the ingestion pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IngestionTelemetry {
    pub events_received: u64,
    pub events_recorded: u64,
    pub events_filtered: u64,
    pub events_rejected: u64,
    pub lifecycle_events: u64,
    pub inbound_events: u64,
    pub outbound_events: u64,
}

/// Telemetry snapshot for the ingestion pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: IngestionTelemetry,
    pub audit_chain_length: usize,
    pub pipeline_config: IngestionPipelineConfig,
}

/// The audit-chain ingestion pipeline.
///
/// Normalizes `CanonicalConnectorEvent`s into `AuditChainEntry`s, applying
/// direction-based filtering, severity gating, and rate limiting before
/// appending to the chain.
pub struct IngestionPipeline {
    config: IngestionPipelineConfig,
    audit_chain: AuditChain,
    telemetry: IngestionTelemetry,
    /// Rate-limiter: last-second event count.
    window_start_ms: u64,
    window_count: u64,
}

impl IngestionPipeline {
    /// Create a new ingestion pipeline.
    pub fn new(config: IngestionPipelineConfig) -> Self {
        let max_entries = config.max_audit_entries;
        Self {
            config,
            audit_chain: AuditChain::new(max_entries),
            telemetry: IngestionTelemetry::default(),
            window_start_ms: 0,
            window_count: 0,
        }
    }

    /// Ingest a canonical connector event into the audit chain.
    pub fn ingest(&mut self, event: &CanonicalConnectorEvent, now_ms: u64) -> IngestionOutcome {
        self.telemetry.events_received += 1;

        // Direction filter.
        match event.direction {
            EventDirection::Inbound if !self.config.ingest_inbound => {
                self.telemetry.events_filtered += 1;
                return IngestionOutcome::Filtered;
            }
            EventDirection::Outbound if !self.config.ingest_outbound => {
                self.telemetry.events_filtered += 1;
                return IngestionOutcome::Filtered;
            }
            EventDirection::Lifecycle if !self.config.ingest_lifecycle => {
                self.telemetry.events_filtered += 1;
                return IngestionOutcome::Filtered;
            }
            _ => {}
        }

        // Severity gate.
        let event_severity = severity_to_level(&event.severity);
        if event_severity < self.config.min_severity_level {
            self.telemetry.events_filtered += 1;
            return IngestionOutcome::Filtered;
        }

        // Validation.
        if event.connector_id.is_empty() {
            self.telemetry.events_rejected += 1;
            return IngestionOutcome::Rejected {
                reason: "empty connector_id".to_string(),
            };
        }
        if event.event_id.is_empty() {
            self.telemetry.events_rejected += 1;
            return IngestionOutcome::Rejected {
                reason: "empty event_id".to_string(),
            };
        }

        // Rate limit.
        if self.config.max_ingest_per_sec > 0 {
            if now_ms >= self.window_start_ms + 1000 {
                self.window_start_ms = now_ms;
                self.window_count = 0;
            }
            if self.window_count >= self.config.max_ingest_per_sec {
                self.telemetry.events_rejected += 1;
                return IngestionOutcome::Rejected {
                    reason: "rate limit exceeded".to_string(),
                };
            }
            self.window_count += 1;
        }

        // Record direction-specific counter.
        match event.direction {
            EventDirection::Lifecycle => self.telemetry.lifecycle_events += 1,
            EventDirection::Inbound => self.telemetry.inbound_events += 1,
            EventDirection::Outbound => self.telemetry.outbound_events += 1,
        }

        // Build audit entry description.
        let description = format!(
            "{} {} event '{}' from connector '{}'",
            event.direction, event.severity, event.event_type, event.connector_id
        );

        // Determine the audit entry kind based on event characteristics.
        let kind = classify_audit_kind(event);

        // Entity reference uses the event's rule_id.
        let entity_ref = event.rule_id();

        // Append to audit chain.
        self.audit_chain.append(
            kind,
            &event.connector_id,
            &description,
            &entity_ref,
            now_ms,
        );

        self.telemetry.events_recorded += 1;
        IngestionOutcome::Recorded
    }

    /// Get a reference to the underlying audit chain.
    #[must_use]
    pub fn audit_chain(&self) -> &AuditChain {
        &self.audit_chain
    }

    /// Get a mutable reference to the audit chain.
    pub fn audit_chain_mut(&mut self) -> &mut AuditChain {
        &mut self.audit_chain
    }

    /// Get telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &IngestionTelemetry {
        &self.telemetry
    }

    /// Capture a telemetry snapshot.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> IngestionTelemetrySnapshot {
        IngestionTelemetrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            audit_chain_length: self.audit_chain.len(),
            pipeline_config: self.config.clone(),
        }
    }

    /// Get the pipeline configuration.
    #[must_use]
    pub fn config(&self) -> &IngestionPipelineConfig {
        &self.config
    }

    /// Verify the integrity of the underlying audit chain.
    pub fn verify_chain(&mut self) -> crate::policy_audit_chain::ChainVerificationResult {
        self.audit_chain.verify()
    }

    /// Export the audit chain as JSON.
    pub fn export_json(&mut self) -> String {
        self.audit_chain.export_json()
    }

    /// Export the audit chain as JSONL.
    pub fn export_jsonl(&mut self) -> String {
        self.audit_chain.export_jsonl()
    }
}

// =============================================================================
// Bundle registry
// =============================================================================

/// Error type for bundle registry operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleRegistryError {
    #[error("bundle not found: {bundle_id}")]
    NotFound { bundle_id: String },

    #[error("bundle already exists: {bundle_id}")]
    AlreadyExists { bundle_id: String },

    #[error("bundle validation failed: {reason}")]
    ValidationFailed { reason: String },

    #[error("connector not found in bundle '{bundle_id}': {package_id}")]
    ConnectorNotInBundle {
        bundle_id: String,
        package_id: String,
    },

    #[error("trust level insufficient: connector '{package_id}' requires {required}, has {actual}")]
    TrustInsufficient {
        package_id: String,
        required: String,
        actual: String,
    },

    #[error("registry capacity exceeded (max {max})")]
    CapacityExceeded { max: usize },
}

/// Registry configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BundleRegistryConfig {
    /// Maximum bundles the registry can hold.
    pub max_bundles: usize,
    /// Maximum audit log entries for the registry itself.
    pub max_audit_entries: usize,
}

impl Default for BundleRegistryConfig {
    fn default() -> Self {
        Self {
            max_bundles: 256,
            max_audit_entries: 512,
        }
    }
}

/// Audit entry for bundle registry operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleAuditEntry {
    /// What happened.
    pub action: BundleAuditAction,
    /// Which bundle.
    pub bundle_id: String,
    /// Who performed it.
    pub actor: String,
    /// When (epoch ms).
    pub timestamp_ms: u64,
    /// Additional details.
    pub detail: String,
}

/// Actions recorded in the bundle audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleAuditAction {
    /// Bundle was registered.
    Registered,
    /// Bundle was updated.
    Updated,
    /// Bundle was removed.
    Removed,
    /// Bundle validation was performed.
    Validated,
    /// A connector was activated from the bundle.
    ConnectorActivated,
    /// A connector was deactivated from the bundle.
    ConnectorDeactivated,
}

impl std::fmt::Display for BundleAuditAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registered => f.write_str("registered"),
            Self::Updated => f.write_str("updated"),
            Self::Removed => f.write_str("removed"),
            Self::Validated => f.write_str("validated"),
            Self::ConnectorActivated => f.write_str("connector_activated"),
            Self::ConnectorDeactivated => f.write_str("connector_deactivated"),
        }
    }
}

/// A registry for managing connector bundles.
pub struct BundleRegistry {
    bundles: BTreeMap<String, ConnectorBundle>,
    config: BundleRegistryConfig,
    audit_log: VecDeque<BundleAuditEntry>,
    telemetry: BundleRegistryTelemetry,
}

/// Telemetry counters for the bundle registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BundleRegistryTelemetry {
    pub bundles_registered: u64,
    pub bundles_removed: u64,
    pub bundles_updated: u64,
    pub validations_run: u64,
    pub validation_failures: u64,
}

/// Telemetry snapshot for the bundle registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRegistrySnapshot {
    pub captured_at_ms: u64,
    pub counters: BundleRegistryTelemetry,
    pub bundle_count: usize,
    pub audit_log_length: usize,
    pub bundles_by_tier: BTreeMap<String, usize>,
    pub bundles_by_category: BTreeMap<String, usize>,
}

impl BundleRegistry {
    /// Create a new bundle registry.
    pub fn new(config: BundleRegistryConfig) -> Self {
        Self {
            bundles: BTreeMap::new(),
            config,
            audit_log: VecDeque::new(),
            telemetry: BundleRegistryTelemetry::default(),
        }
    }

    /// Register a new bundle.
    pub fn register(
        &mut self,
        bundle: ConnectorBundle,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), BundleRegistryError> {
        if self.bundles.len() >= self.config.max_bundles {
            return Err(BundleRegistryError::CapacityExceeded {
                max: self.config.max_bundles,
            });
        }
        if self.bundles.contains_key(&bundle.bundle_id) {
            return Err(BundleRegistryError::AlreadyExists {
                bundle_id: bundle.bundle_id.clone(),
            });
        }

        let validation = validate_bundle(&bundle);
        self.telemetry.validations_run += 1;
        if !validation.valid {
            self.telemetry.validation_failures += 1;
            return Err(BundleRegistryError::ValidationFailed {
                reason: validation.errors.join("; "),
            });
        }

        let bundle_id = bundle.bundle_id.clone();
        self.bundles.insert(bundle_id.clone(), bundle);
        self.telemetry.bundles_registered += 1;
        self.record_audit(BundleAuditAction::Registered, &bundle_id, actor, now_ms, "");
        Ok(())
    }

    /// Update an existing bundle.
    pub fn update(
        &mut self,
        bundle: ConnectorBundle,
        actor: &str,
        now_ms: u64,
    ) -> Result<(), BundleRegistryError> {
        if !self.bundles.contains_key(&bundle.bundle_id) {
            return Err(BundleRegistryError::NotFound {
                bundle_id: bundle.bundle_id.clone(),
            });
        }

        let validation = validate_bundle(&bundle);
        self.telemetry.validations_run += 1;
        if !validation.valid {
            self.telemetry.validation_failures += 1;
            return Err(BundleRegistryError::ValidationFailed {
                reason: validation.errors.join("; "),
            });
        }

        let bundle_id = bundle.bundle_id.clone();
        self.bundles.insert(bundle_id.clone(), bundle);
        self.telemetry.bundles_updated += 1;
        self.record_audit(BundleAuditAction::Updated, &bundle_id, actor, now_ms, "");
        Ok(())
    }

    /// Remove a bundle.
    pub fn remove(
        &mut self,
        bundle_id: &str,
        actor: &str,
        now_ms: u64,
    ) -> Result<ConnectorBundle, BundleRegistryError> {
        let bundle = self
            .bundles
            .remove(bundle_id)
            .ok_or_else(|| BundleRegistryError::NotFound {
                bundle_id: bundle_id.to_string(),
            })?;
        self.telemetry.bundles_removed += 1;
        self.record_audit(BundleAuditAction::Removed, bundle_id, actor, now_ms, "");
        Ok(bundle)
    }

    /// Get a bundle by ID.
    #[must_use]
    pub fn get(&self, bundle_id: &str) -> Option<&ConnectorBundle> {
        self.bundles.get(bundle_id)
    }

    /// List all bundle IDs.
    #[must_use]
    pub fn bundle_ids(&self) -> Vec<&str> {
        self.bundles.keys().map(String::as_str).collect()
    }

    /// Number of registered bundles.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bundles.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty()
    }

    /// Find bundles by tier.
    #[must_use]
    pub fn find_by_tier(&self, tier: BundleTier) -> Vec<&ConnectorBundle> {
        self.bundles
            .values()
            .filter(|b| b.tier == tier)
            .collect()
    }

    /// Find bundles by category.
    #[must_use]
    pub fn find_by_category(&self, category: BundleCategory) -> Vec<&ConnectorBundle> {
        self.bundles
            .values()
            .filter(|b| b.category == category)
            .collect()
    }

    /// Find bundles containing a specific connector package.
    #[must_use]
    pub fn find_by_package(&self, package_id: &str) -> Vec<&ConnectorBundle> {
        self.bundles
            .values()
            .filter(|b| b.connectors.iter().any(|c| c.package_id == package_id))
            .collect()
    }

    /// Get the audit log.
    #[must_use]
    pub fn audit_log(&self) -> &VecDeque<BundleAuditEntry> {
        &self.audit_log
    }

    /// Get telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &BundleRegistryTelemetry {
        &self.telemetry
    }

    /// Capture a snapshot.
    #[must_use]
    pub fn snapshot(&self, now_ms: u64) -> BundleRegistrySnapshot {
        let mut by_tier: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_category: BTreeMap<String, usize> = BTreeMap::new();
        for b in self.bundles.values() {
            *by_tier.entry(b.tier.to_string()).or_default() += 1;
            *by_category.entry(b.category.to_string()).or_default() += 1;
        }
        BundleRegistrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            bundle_count: self.bundles.len(),
            audit_log_length: self.audit_log.len(),
            bundles_by_tier: by_tier,
            bundles_by_category: by_category,
        }
    }

    fn record_audit(
        &mut self,
        action: BundleAuditAction,
        bundle_id: &str,
        actor: &str,
        timestamp_ms: u64,
        detail: &str,
    ) {
        if self.audit_log.len() >= self.config.max_audit_entries {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(BundleAuditEntry {
            action,
            bundle_id: bundle_id.to_string(),
            actor: actor.to_string(),
            timestamp_ms,
            detail: detail.to_string(),
        });
    }
}

// =============================================================================
// Tier-1 built-in bundle definitions
// =============================================================================

/// Create the tier-1 DevTools bundle (GitHub + Linear).
#[must_use]
pub fn tier1_devtools_bundle(now_ms: u64) -> ConnectorBundle {
    let mut bundle = ConnectorBundle::new(
        "ft-bundle-devtools",
        "DevTools",
        BundleTier::Tier1,
        BundleCategory::SourceControl,
        now_ms,
    )
    .with_description("Source control and project management connectors for development workflows")
    .with_version("1.0.0")
    .with_author("frankenterm");

    bundle.add_connector(
        BundleConnectorEntry::required("conn-github", "GitHub")
            .with_min_version("1.0.0")
            .with_capability(ConnectorCapability::Invoke)
            .with_capability(ConnectorCapability::ReadState)
            .with_capability(ConnectorCapability::StreamEvents),
    );
    bundle.add_connector(
        BundleConnectorEntry::required("conn-linear", "Linear")
            .with_min_version("1.0.0")
            .with_capability(ConnectorCapability::Invoke)
            .with_capability(ConnectorCapability::ReadState),
    );
    bundle.add_label("development");
    bundle.add_label("tier1");
    bundle
}

/// Create the tier-1 Communications bundle (Slack + Discord).
#[must_use]
pub fn tier1_comms_bundle(now_ms: u64) -> ConnectorBundle {
    let mut bundle = ConnectorBundle::new(
        "ft-bundle-comms",
        "Communications",
        BundleTier::Tier1,
        BundleCategory::Messaging,
        now_ms,
    )
    .with_description("Messaging and chat connectors for team coordination")
    .with_version("1.0.0")
    .with_author("frankenterm");

    bundle.add_connector(
        BundleConnectorEntry::required("conn-slack", "Slack")
            .with_min_version("1.0.0")
            .with_capability(ConnectorCapability::Invoke)
            .with_capability(ConnectorCapability::StreamEvents),
    );
    bundle.add_connector(
        BundleConnectorEntry::required("conn-discord", "Discord")
            .with_min_version("1.0.0")
            .with_capability(ConnectorCapability::Invoke)
            .with_capability(ConnectorCapability::StreamEvents),
    );
    bundle.add_label("messaging");
    bundle.add_label("tier1");
    bundle
}

/// Create the tier-1 Observability bundle (PagerDuty + monitoring).
#[must_use]
pub fn tier1_observability_bundle(now_ms: u64) -> ConnectorBundle {
    let mut bundle = ConnectorBundle::new(
        "ft-bundle-observability",
        "Observability",
        BundleTier::Tier1,
        BundleCategory::Monitoring,
        now_ms,
    )
    .with_description("Monitoring and incident management connectors for fleet observability")
    .with_version("1.0.0")
    .with_author("frankenterm");

    bundle.add_connector(
        BundleConnectorEntry::required("conn-pagerduty", "PagerDuty")
            .with_min_version("1.0.0")
            .with_capability(ConnectorCapability::Invoke)
            .with_capability(ConnectorCapability::StreamEvents),
    );
    bundle.add_connector(
        BundleConnectorEntry::optional("conn-datadog", "Datadog")
            .with_min_version("1.0.0")
            .with_capability(ConnectorCapability::Invoke),
    );
    bundle.add_label("monitoring");
    bundle.add_label("tier1");
    bundle
}

// =============================================================================
// Helper: map severity to numeric level
// =============================================================================

use crate::connector_event_model::CanonicalSeverity;

fn severity_to_level(severity: &CanonicalSeverity) -> u32 {
    match severity {
        CanonicalSeverity::Info => 0,
        CanonicalSeverity::Warning => 1,
        CanonicalSeverity::Critical => 2,
    }
}

/// Map a canonical event into the appropriate audit entry kind.
fn classify_audit_kind(event: &CanonicalConnectorEvent) -> AuditEntryKind {
    // Lifecycle events map to ConfigChange (connector state transitions).
    if event.direction == EventDirection::Lifecycle {
        return AuditEntryKind::ConfigChange;
    }
    // Credential-related events map to CredentialAction.
    if event.event_type.contains("credential")
        || event.event_type.contains("secret")
        || event.event_type.contains("token")
    {
        return AuditEntryKind::CredentialAction;
    }
    // Quarantine/kill-switch events.
    if event.event_type.contains("quarantine") {
        return AuditEntryKind::QuarantineAction;
    }
    if event.event_type.contains("kill_switch") {
        return AuditEntryKind::KillSwitchAction;
    }
    // Compliance violations.
    if event.event_type.contains("violation") || event.event_type.contains("non_compliant") {
        return AuditEntryKind::ComplianceViolation;
    }
    // Default: treat as a policy decision record.
    AuditEntryKind::PolicyDecision
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(
        connector_id: &str,
        event_type: &str,
        direction: EventDirection,
        severity: CanonicalSeverity,
    ) -> CanonicalConnectorEvent {
        CanonicalConnectorEvent {
            schema_version: SchemaVersion::current(),
            direction,
            event_id: format!("evt-{event_type}-001"),
            correlation_id: "corr-001".to_string(),
            timestamp_ms: 1000,
            connector_id: connector_id.to_string(),
            connector_name: Some(connector_id.to_string()),
            event_type: event_type.to_string(),
            severity,
            signal_kind: None,
            signal_sub_type: None,
            event_source: None,
            action_kind: None,
            lifecycle_phase: None,
            failure_class: None,
            pane_id: None,
            workflow_id: None,
            zone_id: None,
            capability: None,
            payload: serde_json::Value::Null,
            metadata: BTreeMap::new(),
        }
    }

    // -------------------------------------------------------------------------
    // BundleTier tests
    // -------------------------------------------------------------------------

    #[test]
    fn tier_minimum_trust() {
        assert_eq!(BundleTier::Tier1.minimum_trust(), TrustLevel::Trusted);
        assert_eq!(BundleTier::Tier2.minimum_trust(), TrustLevel::Trusted);
        assert_eq!(BundleTier::Tier3.minimum_trust(), TrustLevel::Conditional);
        assert_eq!(BundleTier::Custom.minimum_trust(), TrustLevel::Untrusted);
    }

    #[test]
    fn tier_signed_requirement() {
        assert!(BundleTier::Tier1.requires_signed_manifest());
        assert!(BundleTier::Tier2.requires_signed_manifest());
        assert!(!BundleTier::Tier3.requires_signed_manifest());
        assert!(!BundleTier::Custom.requires_signed_manifest());
    }

    #[test]
    fn tier_display() {
        assert_eq!(BundleTier::Tier1.to_string(), "tier1");
        assert_eq!(BundleTier::Custom.to_string(), "custom");
    }

    #[test]
    fn tier_ord() {
        assert!(BundleTier::Tier1 < BundleTier::Tier2);
        assert!(BundleTier::Tier2 < BundleTier::Tier3);
        assert!(BundleTier::Tier3 < BundleTier::Custom);
    }

    #[test]
    fn tier_serde_roundtrip() {
        let tier = BundleTier::Tier1;
        let json = serde_json::to_string(&tier).unwrap();
        let back: BundleTier = serde_json::from_str(&json).unwrap();
        assert_eq!(tier, back);
    }

    // -------------------------------------------------------------------------
    // BundleCategory tests
    // -------------------------------------------------------------------------

    #[test]
    fn category_display() {
        assert_eq!(BundleCategory::SourceControl.to_string(), "source_control");
        assert_eq!(BundleCategory::Messaging.to_string(), "messaging");
        assert_eq!(BundleCategory::CiCd.to_string(), "ci_cd");
    }

    #[test]
    fn category_serde_roundtrip() {
        let cat = BundleCategory::ProjectManagement;
        let json = serde_json::to_string(&cat).unwrap();
        let back: BundleCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(cat, back);
    }

    // -------------------------------------------------------------------------
    // BundleConnectorEntry tests
    // -------------------------------------------------------------------------

    #[test]
    fn connector_entry_required() {
        let entry = BundleConnectorEntry::required("conn-github", "GitHub");
        assert!(entry.required);
        assert_eq!(entry.package_id, "conn-github");
        assert_eq!(entry.display_name, "GitHub");
    }

    #[test]
    fn connector_entry_optional() {
        let entry = BundleConnectorEntry::optional("conn-datadog", "Datadog");
        assert!(!entry.required);
    }

    #[test]
    fn connector_entry_builder() {
        let entry = BundleConnectorEntry::required("conn-github", "GitHub")
            .with_min_version("2.0.0")
            .with_capability(ConnectorCapability::Invoke)
            .with_capability(ConnectorCapability::ReadState)
            .with_capability(ConnectorCapability::Invoke); // dedup
        assert_eq!(entry.min_version, "2.0.0");
        assert_eq!(entry.required_capabilities.len(), 2);
    }

    #[test]
    fn connector_entry_serde_roundtrip() {
        let entry = BundleConnectorEntry::required("conn-slack", "Slack")
            .with_min_version("1.0.0");
        let json = serde_json::to_string(&entry).unwrap();
        let back: BundleConnectorEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    // -------------------------------------------------------------------------
    // ConnectorBundle tests
    // -------------------------------------------------------------------------

    #[test]
    fn bundle_creation() {
        let bundle = ConnectorBundle::new(
            "test-bundle",
            "Test",
            BundleTier::Tier1,
            BundleCategory::General,
            1000,
        );
        assert_eq!(bundle.bundle_id, "test-bundle");
        assert_eq!(bundle.connector_count(), 0);
        assert_eq!(bundle.required_count(), 0);
        assert_eq!(bundle.optional_count(), 0);
    }

    #[test]
    fn bundle_add_connectors() {
        let mut bundle = ConnectorBundle::new(
            "test-bundle",
            "Test",
            BundleTier::Tier1,
            BundleCategory::General,
            1000,
        );
        bundle.add_connector(BundleConnectorEntry::required("a", "A"));
        bundle.add_connector(BundleConnectorEntry::optional("b", "B"));
        assert_eq!(bundle.connector_count(), 2);
        assert_eq!(bundle.required_count(), 1);
        assert_eq!(bundle.optional_count(), 1);
        assert_eq!(bundle.package_ids(), vec!["a", "b"]);
    }

    #[test]
    fn bundle_serde_roundtrip() {
        let bundle = tier1_devtools_bundle(5000);
        let json = serde_json::to_string(&bundle).unwrap();
        let back: ConnectorBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(bundle, back);
    }

    #[test]
    fn bundle_capabilities_union() {
        let mut bundle = ConnectorBundle::new(
            "test",
            "Test",
            BundleTier::Tier1,
            BundleCategory::General,
            1000,
        );
        bundle.add_connector(
            BundleConnectorEntry::required("a", "A")
                .with_capability(ConnectorCapability::Invoke)
                .with_capability(ConnectorCapability::ReadState),
        );
        bundle.add_connector(
            BundleConnectorEntry::required("b", "B")
                .with_capability(ConnectorCapability::StreamEvents),
        );
        let caps = bundle.all_required_capabilities();
        assert_eq!(caps.len(), 3);
    }

    // -------------------------------------------------------------------------
    // Validation tests
    // -------------------------------------------------------------------------

    #[test]
    fn validate_valid_bundle() {
        let bundle = tier1_devtools_bundle(1000);
        let result = validate_bundle(&bundle);
        assert!(result.valid);
    }

    #[test]
    fn validate_empty_id() {
        let bundle = ConnectorBundle::new("", "Test", BundleTier::Tier1, BundleCategory::General, 0);
        let result = validate_bundle(&bundle);
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("bundle_id")));
    }

    #[test]
    fn validate_no_connectors() {
        let bundle = ConnectorBundle::new(
            "test",
            "Test",
            BundleTier::Custom,
            BundleCategory::General,
            0,
        );
        let result = validate_bundle(&bundle);
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("no connectors")));
    }

    #[test]
    fn validate_duplicate_package_ids() {
        let mut bundle = ConnectorBundle::new(
            "test",
            "Test",
            BundleTier::Custom,
            BundleCategory::General,
            0,
        );
        bundle.add_connector(BundleConnectorEntry::required("dup", "Dup1"));
        bundle.add_connector(BundleConnectorEntry::required("dup", "Dup2"));
        let result = validate_bundle(&bundle);
        assert!(!result.valid);
        assert!(result.errors.iter().any(|e| e.contains("duplicate")));
    }

    #[test]
    fn validate_display() {
        let mut r = BundleValidationResult::ok();
        assert_eq!(r.to_string(), "valid");
        r.warn("something");
        assert!(r.to_string().contains("1 warnings"));
        r.error("bad");
        assert!(r.to_string().contains("INVALID"));
    }

    // -------------------------------------------------------------------------
    // BundleRegistry tests
    // -------------------------------------------------------------------------

    #[test]
    fn registry_register_and_get() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let bundle = tier1_devtools_bundle(1000);
        reg.register(bundle, "agent-1", 1000).unwrap();
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
        assert!(reg.get("ft-bundle-devtools").is_some());
    }

    #[test]
    fn registry_duplicate_rejected() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let b1 = tier1_devtools_bundle(1000);
        let b2 = tier1_devtools_bundle(2000);
        reg.register(b1, "agent-1", 1000).unwrap();
        let err = reg.register(b2, "agent-1", 2000).unwrap_err();
        assert!(matches!(err, BundleRegistryError::AlreadyExists { .. }));
    }

    #[test]
    fn registry_update() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let b = tier1_devtools_bundle(1000);
        reg.register(b, "agent-1", 1000).unwrap();

        let mut updated = tier1_devtools_bundle(2000);
        updated.version = "2.0.0".to_string();
        reg.update(updated, "agent-1", 2000).unwrap();
        assert_eq!(reg.get("ft-bundle-devtools").unwrap().version, "2.0.0");
    }

    #[test]
    fn registry_update_nonexistent() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let b = tier1_devtools_bundle(1000);
        let err = reg.update(b, "agent-1", 1000).unwrap_err();
        assert!(matches!(err, BundleRegistryError::NotFound { .. }));
    }

    #[test]
    fn registry_remove() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "agent-1", 1000)
            .unwrap();
        let removed = reg.remove("ft-bundle-devtools", "agent-1", 2000).unwrap();
        assert_eq!(removed.bundle_id, "ft-bundle-devtools");
        assert!(reg.is_empty());
    }

    #[test]
    fn registry_remove_nonexistent() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        let err = reg.remove("nope", "agent-1", 1000).unwrap_err();
        assert!(matches!(err, BundleRegistryError::NotFound { .. }));
    }

    #[test]
    fn registry_capacity_enforced() {
        let config = BundleRegistryConfig {
            max_bundles: 1,
            max_audit_entries: 10,
        };
        let mut reg = BundleRegistry::new(config);
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        let err = reg
            .register(tier1_comms_bundle(2000), "a", 2000)
            .unwrap_err();
        assert!(matches!(err, BundleRegistryError::CapacityExceeded { .. }));
    }

    #[test]
    fn registry_find_by_tier() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        reg.register(tier1_comms_bundle(2000), "a", 2000).unwrap();
        let tier1 = reg.find_by_tier(BundleTier::Tier1);
        assert_eq!(tier1.len(), 2);
        let tier2 = reg.find_by_tier(BundleTier::Tier2);
        assert!(tier2.is_empty());
    }

    #[test]
    fn registry_find_by_category() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        reg.register(tier1_comms_bundle(2000), "a", 2000).unwrap();
        let source = reg.find_by_category(BundleCategory::SourceControl);
        assert_eq!(source.len(), 1);
        let messaging = reg.find_by_category(BundleCategory::Messaging);
        assert_eq!(messaging.len(), 1);
    }

    #[test]
    fn registry_find_by_package() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        reg.register(tier1_comms_bundle(2000), "a", 2000).unwrap();
        let github = reg.find_by_package("conn-github");
        assert_eq!(github.len(), 1);
        let slack = reg.find_by_package("conn-slack");
        assert_eq!(slack.len(), 1);
        let nope = reg.find_by_package("conn-nope");
        assert!(nope.is_empty());
    }

    #[test]
    fn registry_audit_log() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "agent-1", 1000)
            .unwrap();
        reg.remove("ft-bundle-devtools", "agent-1", 2000).unwrap();
        assert_eq!(reg.audit_log().len(), 2);
        assert_eq!(reg.audit_log()[0].action, BundleAuditAction::Registered);
        assert_eq!(reg.audit_log()[1].action, BundleAuditAction::Removed);
    }

    #[test]
    fn registry_audit_log_bounded() {
        let config = BundleRegistryConfig {
            max_bundles: 256,
            max_audit_entries: 2,
        };
        let mut reg = BundleRegistry::new(config);
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        reg.register(tier1_comms_bundle(2000), "a", 2000).unwrap();
        reg.register(tier1_observability_bundle(3000), "a", 3000)
            .unwrap();
        // 3 registrations but max_audit_entries=2, so oldest evicted.
        assert_eq!(reg.audit_log().len(), 2);
    }

    #[test]
    fn registry_telemetry() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        let t = reg.telemetry();
        assert_eq!(t.bundles_registered, 1);
        assert_eq!(t.validations_run, 1);
    }

    #[test]
    fn registry_snapshot() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        reg.register(tier1_comms_bundle(2000), "a", 2000).unwrap();
        let snap = reg.snapshot(3000);
        assert_eq!(snap.bundle_count, 2);
        assert_eq!(snap.captured_at_ms, 3000);
        assert_eq!(*snap.bundles_by_tier.get("tier1").unwrap_or(&0), 2);
    }

    #[test]
    fn registry_bundle_ids() {
        let mut reg = BundleRegistry::new(BundleRegistryConfig::default());
        reg.register(tier1_devtools_bundle(1000), "a", 1000).unwrap();
        reg.register(tier1_comms_bundle(2000), "a", 2000).unwrap();
        let ids = reg.bundle_ids();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"ft-bundle-devtools"));
        assert!(ids.contains(&"ft-bundle-comms"));
    }

    // -------------------------------------------------------------------------
    // IngestionPipeline tests
    // -------------------------------------------------------------------------

    #[test]
    fn ingest_records_event() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        let outcome = pipeline.ingest(&event, 1000);
        assert_eq!(outcome, IngestionOutcome::Recorded);
        assert_eq!(pipeline.telemetry().events_received, 1);
        assert_eq!(pipeline.telemetry().events_recorded, 1);
        assert_eq!(pipeline.telemetry().inbound_events, 1);
    }

    #[test]
    fn ingest_filters_disabled_direction() {
        let config = IngestionPipelineConfig {
            ingest_inbound: false,
            ..Default::default()
        };
        let mut pipeline = IngestionPipeline::new(config);
        let event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        let outcome = pipeline.ingest(&event, 1000);
        assert_eq!(outcome, IngestionOutcome::Filtered);
        assert_eq!(pipeline.telemetry().events_filtered, 1);
    }

    #[test]
    fn ingest_filters_low_severity() {
        let config = IngestionPipelineConfig {
            min_severity_level: 2, // Critical only
            ..Default::default()
        };
        let mut pipeline = IngestionPipeline::new(config);
        let event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        let outcome = pipeline.ingest(&event, 1000);
        assert_eq!(outcome, IngestionOutcome::Filtered);
    }

    #[test]
    fn ingest_rejects_empty_connector_id() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let event = sample_event("", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        let outcome = pipeline.ingest(&event, 1000);
        assert!(matches!(outcome, IngestionOutcome::Rejected { .. }));
        assert_eq!(pipeline.telemetry().events_rejected, 1);
    }

    #[test]
    fn ingest_rejects_empty_event_id() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let mut event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        event.event_id = String::new();
        let outcome = pipeline.ingest(&event, 1000);
        assert!(matches!(outcome, IngestionOutcome::Rejected { .. }));
    }

    #[test]
    fn ingest_rate_limiting() {
        let config = IngestionPipelineConfig {
            max_ingest_per_sec: 2,
            ..Default::default()
        };
        let mut pipeline = IngestionPipeline::new(config);
        let event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        assert_eq!(pipeline.ingest(&event, 1000), IngestionOutcome::Recorded);
        assert_eq!(pipeline.ingest(&event, 1001), IngestionOutcome::Recorded);
        // Third in same second window → rejected.
        let outcome = pipeline.ingest(&event, 1002);
        assert!(matches!(outcome, IngestionOutcome::Rejected { .. }));
        // New window at 2000ms.
        assert_eq!(pipeline.ingest(&event, 2000), IngestionOutcome::Recorded);
    }

    #[test]
    fn ingest_lifecycle_event_recorded() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let event = sample_event(
            "conn-github",
            "state_change",
            EventDirection::Lifecycle,
            CanonicalSeverity::Info,
        );
        let outcome = pipeline.ingest(&event, 1000);
        assert_eq!(outcome, IngestionOutcome::Recorded);
        assert_eq!(pipeline.telemetry().lifecycle_events, 1);
    }

    #[test]
    fn ingest_outbound_event_recorded() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let event = sample_event(
            "conn-slack",
            "send_message",
            EventDirection::Outbound,
            CanonicalSeverity::Info,
        );
        let outcome = pipeline.ingest(&event, 1000);
        assert_eq!(outcome, IngestionOutcome::Recorded);
        assert_eq!(pipeline.telemetry().outbound_events, 1);
    }

    #[test]
    fn ingest_chain_integrity() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        for i in 0..10 {
            let event = sample_event(
                "conn-github",
                &format!("event_{i}"),
                EventDirection::Inbound,
                CanonicalSeverity::Info,
            );
            pipeline.ingest(&event, 1000 + i * 100);
        }
        let verify = pipeline.verify_chain();
        assert!(verify.valid);
        assert_eq!(verify.entries_checked, 10);
    }

    #[test]
    fn ingest_snapshot() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        pipeline.ingest(&event, 1000);
        let snap = pipeline.snapshot(2000);
        assert_eq!(snap.captured_at_ms, 2000);
        assert_eq!(snap.counters.events_recorded, 1);
        assert_eq!(snap.audit_chain_length, 1);
    }

    #[test]
    fn ingest_export_formats() {
        let mut pipeline = IngestionPipeline::new(IngestionPipelineConfig::default());
        let event = sample_event("conn-github", "push", EventDirection::Inbound, CanonicalSeverity::Info);
        pipeline.ingest(&event, 1000);
        let json = pipeline.export_json();
        assert!(json.contains("conn-github"));
        let jsonl = pipeline.export_jsonl();
        assert!(jsonl.contains("conn-github"));
    }

    #[test]
    fn classify_lifecycle_as_config_change() {
        let event = sample_event("c", "init", EventDirection::Lifecycle, CanonicalSeverity::Info);
        assert_eq!(classify_audit_kind(&event), AuditEntryKind::ConfigChange);
    }

    #[test]
    fn classify_credential_event() {
        let event = sample_event("c", "credential_rotated", EventDirection::Inbound, CanonicalSeverity::Info);
        assert_eq!(classify_audit_kind(&event), AuditEntryKind::CredentialAction);
    }

    #[test]
    fn classify_quarantine_event() {
        let event = sample_event("c", "quarantine_applied", EventDirection::Outbound, CanonicalSeverity::Critical);
        assert_eq!(classify_audit_kind(&event), AuditEntryKind::QuarantineAction);
    }

    #[test]
    fn classify_compliance_violation() {
        let event = sample_event("c", "policy_violation", EventDirection::Inbound, CanonicalSeverity::Warning);
        assert_eq!(classify_audit_kind(&event), AuditEntryKind::ComplianceViolation);
    }

    #[test]
    fn classify_default_policy_decision() {
        let event = sample_event("c", "action_completed", EventDirection::Outbound, CanonicalSeverity::Info);
        assert_eq!(classify_audit_kind(&event), AuditEntryKind::PolicyDecision);
    }

    // -------------------------------------------------------------------------
    // Tier-1 bundle factory tests
    // -------------------------------------------------------------------------

    #[test]
    fn tier1_devtools_valid() {
        let bundle = tier1_devtools_bundle(1000);
        assert_eq!(bundle.bundle_id, "ft-bundle-devtools");
        assert_eq!(bundle.tier, BundleTier::Tier1);
        assert_eq!(bundle.category, BundleCategory::SourceControl);
        assert_eq!(bundle.connector_count(), 2);
        assert_eq!(bundle.required_count(), 2);
        assert!(validate_bundle(&bundle).valid);
    }

    #[test]
    fn tier1_comms_valid() {
        let bundle = tier1_comms_bundle(1000);
        assert_eq!(bundle.bundle_id, "ft-bundle-comms");
        assert_eq!(bundle.tier, BundleTier::Tier1);
        assert_eq!(bundle.category, BundleCategory::Messaging);
        assert_eq!(bundle.connector_count(), 2);
        assert!(validate_bundle(&bundle).valid);
    }

    #[test]
    fn tier1_observability_valid() {
        let bundle = tier1_observability_bundle(1000);
        assert_eq!(bundle.bundle_id, "ft-bundle-observability");
        assert_eq!(bundle.tier, BundleTier::Tier1);
        assert_eq!(bundle.category, BundleCategory::Monitoring);
        assert_eq!(bundle.connector_count(), 2);
        assert_eq!(bundle.required_count(), 1);
        assert_eq!(bundle.optional_count(), 1);
        assert!(validate_bundle(&bundle).valid);
    }

    // -------------------------------------------------------------------------
    // IngestionOutcome tests
    // -------------------------------------------------------------------------

    #[test]
    fn ingestion_outcome_display() {
        assert_eq!(IngestionOutcome::Recorded.to_string(), "recorded");
        assert_eq!(IngestionOutcome::Filtered.to_string(), "filtered");
        let rejected = IngestionOutcome::Rejected {
            reason: "bad".to_string(),
        };
        assert_eq!(rejected.to_string(), "rejected: bad");
    }

    #[test]
    fn ingestion_outcome_serde() {
        let outcomes = vec![
            IngestionOutcome::Recorded,
            IngestionOutcome::Filtered,
            IngestionOutcome::Rejected { reason: "test".to_string() },
        ];
        for outcome in outcomes {
            let json = serde_json::to_string(&outcome).unwrap();
            let back: IngestionOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, back);
        }
    }

    // -------------------------------------------------------------------------
    // BundleAuditAction tests
    // -------------------------------------------------------------------------

    #[test]
    fn bundle_audit_action_display() {
        assert_eq!(BundleAuditAction::Registered.to_string(), "registered");
        assert_eq!(BundleAuditAction::Updated.to_string(), "updated");
        assert_eq!(BundleAuditAction::Removed.to_string(), "removed");
        assert_eq!(BundleAuditAction::Validated.to_string(), "validated");
        assert_eq!(
            BundleAuditAction::ConnectorActivated.to_string(),
            "connector_activated"
        );
        assert_eq!(
            BundleAuditAction::ConnectorDeactivated.to_string(),
            "connector_deactivated"
        );
    }

    #[test]
    fn bundle_audit_action_serde() {
        let action = BundleAuditAction::Registered;
        let json = serde_json::to_string(&action).unwrap();
        let back: BundleAuditAction = serde_json::from_str(&json).unwrap();
        assert_eq!(action, back);
    }

    // -------------------------------------------------------------------------
    // BundleRegistryError tests
    // -------------------------------------------------------------------------

    #[test]
    fn registry_error_display() {
        let err = BundleRegistryError::NotFound {
            bundle_id: "test".to_string(),
        };
        assert!(err.to_string().contains("test"));

        let err = BundleRegistryError::CapacityExceeded { max: 10 };
        assert!(err.to_string().contains("10"));
    }
}
