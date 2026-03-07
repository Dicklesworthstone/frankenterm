//! Data classification, redaction, and privacy-preserving event ingestion (ft-3681t.5.14).
//!
//! Provides payload classification and redaction controls so connector data
//! entering logs, search indices, and policy stores is privacy-safe and
//! compliance-ready without losing operational utility.
//!
//! # Architecture
//!
//! Three layers work together:
//!
//! 1. **Classification** — Each field/payload gets a sensitivity level
//!    (Public → Internal → Confidential → Restricted → Prohibited).
//! 2. **Redaction** — Strategies (mask, hash, truncate, remove, tokenize)
//!    applied per classification level before ingestion.
//! 3. **Ingestion filter** — Pre-store gate that enforces classification
//!    policy, drops prohibited data, and emits audit entries.
//!
//! # Usage
//!
//! ```ignore
//! use frankenterm_core::connector_data_classification::*;
//!
//! let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
//! classifier.register_policy("github-connector", ClassificationPolicy::default());
//!
//! let event = /* CanonicalConnectorEvent */;
//! let classified = classifier.classify_event(&event);
//! let redacted = classifier.redact_event(&classified);
//! ```

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::connector_event_model::CanonicalConnectorEvent;
use crate::policy::Redactor;

// =============================================================================
// Data sensitivity levels
// =============================================================================

/// Data sensitivity classification level (ordered from least to most sensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataSensitivity {
    /// Safe for public exposure (event types, timestamps, counts).
    Public,
    /// Internal operational data (connector IDs, zone names).
    Internal,
    /// Business-sensitive (workflow details, user content summaries).
    Confidential,
    /// Highly sensitive (credentials, tokens, PII).
    Restricted,
    /// Must never be stored or transmitted (raw secrets, plaintext passwords).
    Prohibited,
}

impl DataSensitivity {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Confidential => "confidential",
            Self::Restricted => "restricted",
            Self::Prohibited => "prohibited",
        }
    }

    /// Whether this level requires redaction before storage.
    #[must_use]
    pub const fn requires_redaction(self) -> bool {
        matches!(
            self,
            Self::Confidential | Self::Restricted | Self::Prohibited
        )
    }

    /// Whether this level must be completely removed (never stored).
    #[must_use]
    pub const fn must_remove(self) -> bool {
        matches!(self, Self::Prohibited)
    }
}

impl std::fmt::Display for DataSensitivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Redaction strategies
// =============================================================================

/// How to redact sensitive data before storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionStrategy {
    /// Replace with a fixed marker string (e.g., "[REDACTED]").
    Mask,
    /// Replace with a deterministic one-way hash (preserves correlation).
    Hash,
    /// Truncate to N characters with "[...]" suffix.
    Truncate { max_len: usize },
    /// Remove the field entirely from the output.
    Remove,
    /// Replace with a reversible token (for authorized later retrieval).
    Tokenize { token_prefix: String },
    /// Pass through without modification.
    Passthrough,
}

impl RedactionStrategy {
    /// Default strategy for a given sensitivity level.
    #[must_use]
    pub fn for_sensitivity(level: DataSensitivity) -> Self {
        match level {
            DataSensitivity::Public => Self::Passthrough,
            DataSensitivity::Internal => Self::Passthrough,
            DataSensitivity::Confidential => Self::Truncate { max_len: 64 },
            DataSensitivity::Restricted => Self::Mask,
            DataSensitivity::Prohibited => Self::Remove,
        }
    }
}

impl std::fmt::Display for RedactionStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mask => f.write_str("mask"),
            Self::Hash => f.write_str("hash"),
            Self::Truncate { max_len } => write!(f, "truncate({max_len})"),
            Self::Remove => f.write_str("remove"),
            Self::Tokenize { token_prefix } => write!(f, "tokenize({token_prefix})"),
            Self::Passthrough => f.write_str("passthrough"),
        }
    }
}

// =============================================================================
// Classification rules
// =============================================================================

/// A rule that matches fields and assigns classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationRule {
    /// Rule identifier.
    pub rule_id: String,
    /// Human-readable description.
    pub description: String,
    /// Field name patterns this rule matches (exact or prefix with `*`).
    pub field_patterns: Vec<String>,
    /// Content patterns that trigger this rule (substring match).
    pub content_patterns: Vec<String>,
    /// Sensitivity level to assign when matched.
    pub sensitivity: DataSensitivity,
    /// Override redaction strategy (uses default for sensitivity if None).
    pub redaction_override: Option<RedactionStrategy>,
    /// Priority (higher = checked first; equal priority = first match wins).
    pub priority: u32,
    /// Whether this rule is enabled.
    pub enabled: bool,
}

impl ClassificationRule {
    /// Create a new classification rule.
    #[must_use]
    pub fn new(
        rule_id: impl Into<String>,
        sensitivity: DataSensitivity,
        field_patterns: Vec<String>,
    ) -> Self {
        Self {
            rule_id: rule_id.into(),
            description: String::new(),
            field_patterns,
            content_patterns: Vec::new(),
            sensitivity,
            redaction_override: None,
            priority: 100,
            enabled: true,
        }
    }

    /// Check if this rule matches a field name.
    #[must_use]
    pub fn matches_field(&self, field_name: &str) -> bool {
        self.field_patterns.iter().any(|pattern| {
            if let Some(prefix) = pattern.strip_suffix('*') {
                field_name.starts_with(prefix)
            } else {
                field_name == pattern
            }
        })
    }

    /// Check if this rule matches field content.
    #[must_use]
    pub fn matches_content(&self, content: &str) -> bool {
        if self.content_patterns.is_empty() {
            return true; // No content patterns = match on field name only
        }
        self.content_patterns
            .iter()
            .any(|pat| content.contains(pat.as_str()))
    }

    /// Get the effective redaction strategy.
    #[must_use]
    pub fn effective_strategy(&self) -> RedactionStrategy {
        self.redaction_override
            .clone()
            .unwrap_or_else(|| RedactionStrategy::for_sensitivity(self.sensitivity))
    }
}

// =============================================================================
// Per-field classification result
// =============================================================================

/// Classification result for a single field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldClassification {
    /// The field path (e.g., "payload.credentials.token").
    pub field_path: String,
    /// Assigned sensitivity level.
    pub sensitivity: DataSensitivity,
    /// Which rule matched (if any; "default" for unmatched fields).
    pub matched_rule: String,
    /// Strategy that will be applied.
    pub strategy: RedactionStrategy,
}

// =============================================================================
// Classification policy
// =============================================================================

/// Per-connector classification policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationPolicy {
    /// Policy identifier.
    pub policy_id: String,
    /// Connector ID this policy applies to (or "*" for default).
    pub connector_pattern: String,
    /// Ordered rules (checked by priority descending, then insertion order).
    pub rules: Vec<ClassificationRule>,
    /// Default sensitivity for unmatched fields.
    pub default_sensitivity: DataSensitivity,
    /// Whether to apply secret-pattern scanning on unclassified string values.
    pub scan_for_secrets: bool,
    /// Maximum payload size to ingest (bytes). Oversized payloads get truncated.
    pub max_payload_bytes: usize,
    /// Whether to allow prohibited-level data through (emergency override).
    pub allow_prohibited: bool,
}

impl Default for ClassificationPolicy {
    fn default() -> Self {
        Self {
            policy_id: "default".to_string(),
            connector_pattern: "*".to_string(),
            rules: default_classification_rules(),
            default_sensitivity: DataSensitivity::Internal,
            scan_for_secrets: true,
            max_payload_bytes: 1_048_576, // 1 MiB
            allow_prohibited: false,
        }
    }
}

impl ClassificationPolicy {
    /// Check if this policy matches a connector ID.
    #[must_use]
    pub fn matches_connector(&self, connector_id: &str) -> bool {
        if self.connector_pattern == "*" {
            return true;
        }
        if let Some(prefix) = self.connector_pattern.strip_suffix('*') {
            connector_id.starts_with(prefix)
        } else {
            self.connector_pattern == connector_id
        }
    }
}

/// Default classification rules covering common sensitive patterns.
fn default_classification_rules() -> Vec<ClassificationRule> {
    vec![
        // Prohibited: raw secrets
        ClassificationRule {
            rule_id: "builtin-secrets".into(),
            description: "Raw secrets, tokens, API keys".into(),
            field_patterns: vec![
                "password".into(),
                "secret".into(),
                "private_key".into(),
                "api_key".into(),
                "access_token".into(),
                "refresh_token".into(),
            ],
            content_patterns: vec![],
            sensitivity: DataSensitivity::Prohibited,
            redaction_override: None,
            priority: 1000,
            enabled: true,
        },
        // Restricted: credentials, PII
        ClassificationRule {
            rule_id: "builtin-credentials".into(),
            description: "Credential references and PII".into(),
            field_patterns: vec![
                "credential*".into(),
                "auth*".into(),
                "email".into(),
                "phone".into(),
                "ssn".into(),
                "address".into(),
            ],
            content_patterns: vec![],
            sensitivity: DataSensitivity::Restricted,
            redaction_override: None,
            priority: 900,
            enabled: true,
        },
        // Confidential: user content
        ClassificationRule {
            rule_id: "builtin-user-content".into(),
            description: "User-generated content and commands".into(),
            field_patterns: vec![
                "command*".into(),
                "input*".into(),
                "output*".into(),
                "body".into(),
                "message".into(),
                "content".into(),
                "diff".into(),
                "patch".into(),
            ],
            content_patterns: vec![],
            sensitivity: DataSensitivity::Confidential,
            redaction_override: None,
            priority: 500,
            enabled: true,
        },
        // Public: structural metadata
        ClassificationRule {
            rule_id: "builtin-structural".into(),
            description: "Structural metadata always safe to store".into(),
            field_patterns: vec![
                "event_type".into(),
                "event_id".into(),
                "timestamp*".into(),
                "direction".into(),
                "severity".into(),
                "schema_version".into(),
                "connector_id".into(),
                "zone_id".into(),
                "pane_id".into(),
                "workflow_id".into(),
                "correlation_id".into(),
            ],
            content_patterns: vec![],
            sensitivity: DataSensitivity::Public,
            redaction_override: None,
            priority: 200,
            enabled: true,
        },
    ]
}

// =============================================================================
// Classified event wrapper
// =============================================================================

/// A connector event annotated with classification results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifiedEvent {
    /// The original event (before redaction).
    pub event_id: String,
    /// Connector that produced the event.
    pub connector_id: String,
    /// Overall sensitivity (highest across all fields).
    pub overall_sensitivity: DataSensitivity,
    /// Per-field classification results.
    pub field_classifications: Vec<FieldClassification>,
    /// Policy that was applied.
    pub policy_id: String,
    /// Whether secret scanning detected patterns.
    pub secrets_detected: bool,
    /// Timestamp of classification (millis since epoch).
    pub classified_at_ms: u64,
}

impl ClassifiedEvent {
    /// Count of fields at each sensitivity level.
    #[must_use]
    pub fn sensitivity_histogram(&self) -> BTreeMap<DataSensitivity, usize> {
        let mut hist = BTreeMap::new();
        for fc in &self.field_classifications {
            *hist.entry(fc.sensitivity).or_insert(0) += 1;
        }
        hist
    }

    /// Whether any field is prohibited.
    #[must_use]
    pub fn has_prohibited(&self) -> bool {
        self.field_classifications
            .iter()
            .any(|fc| fc.sensitivity == DataSensitivity::Prohibited)
    }

    /// Whether any field requires redaction.
    #[must_use]
    pub fn requires_redaction(&self) -> bool {
        self.field_classifications
            .iter()
            .any(|fc| fc.sensitivity.requires_redaction())
    }
}

// =============================================================================
// Redacted event output
// =============================================================================

/// A connector event after redaction has been applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactedEvent {
    /// The underlying canonical event with sensitive data removed/masked.
    pub event: CanonicalConnectorEvent,
    /// Classification summary.
    pub classification: ClassifiedEvent,
    /// Redaction actions taken.
    pub redaction_actions: Vec<RedactionAction>,
}

/// Record of a single redaction action taken.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedactionAction {
    /// Field that was redacted.
    pub field_path: String,
    /// Strategy applied.
    pub strategy: RedactionStrategy,
    /// Original data sensitivity.
    pub sensitivity: DataSensitivity,
    /// Original value length in bytes (for audit, not the value itself).
    pub original_bytes: usize,
}

impl RedactedEvent {
    /// Count of fields that were redacted but retained in sanitized form.
    #[must_use]
    pub fn fields_redacted(&self) -> u32 {
        self.redaction_actions
            .iter()
            .filter(|action| !matches!(action.strategy, RedactionStrategy::Remove))
            .count() as u32
    }

    /// Count of fields that were removed entirely.
    #[must_use]
    pub fn fields_removed(&self) -> u32 {
        self.redaction_actions
            .iter()
            .filter(|action| matches!(action.strategy, RedactionStrategy::Remove))
            .count() as u32
    }
}

// =============================================================================
// Ingestion decision
// =============================================================================

/// Decision about whether an event should be ingested.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestionDecision {
    /// Ingest the event as-is (all fields public/internal).
    Accept,
    /// Ingest after applying redaction.
    AcceptRedacted,
    /// Reject the event entirely (prohibited data, policy violation).
    Reject { reason: String },
    /// Quarantine for manual review.
    Quarantine { reason: String },
}

impl IngestionDecision {
    #[must_use]
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accept | Self::AcceptRedacted)
    }

    #[must_use]
    pub fn is_rejected(&self) -> bool {
        matches!(self, Self::Reject { .. })
    }
}

impl std::fmt::Display for IngestionDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Accept => f.write_str("accept"),
            Self::AcceptRedacted => f.write_str("accept_redacted"),
            Self::Reject { reason } => write!(f, "reject: {reason}"),
            Self::Quarantine { reason } => write!(f, "quarantine: {reason}"),
        }
    }
}

// =============================================================================
// Audit entries
// =============================================================================

/// Audit entry for a classification/redaction decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationAuditEntry {
    /// Event that was classified.
    pub event_id: String,
    /// Connector source.
    pub connector_id: String,
    /// Policy used.
    pub policy_id: String,
    /// Overall sensitivity determined.
    pub sensitivity: DataSensitivity,
    /// Ingestion decision.
    pub decision: IngestionDecision,
    /// Number of fields redacted.
    pub fields_redacted: u32,
    /// Number of fields removed.
    pub fields_removed: u32,
    /// Whether secret scanning triggered.
    pub secrets_detected: bool,
    /// Timestamp (millis since epoch).
    pub timestamp_ms: u64,
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for the data classification subsystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClassificationTelemetry {
    pub events_classified: u64,
    pub events_accepted: u64,
    pub events_accepted_redacted: u64,
    pub events_rejected: u64,
    pub events_quarantined: u64,
    pub fields_classified: u64,
    pub fields_redacted: u64,
    pub fields_removed: u64,
    pub secrets_detected: u64,
    pub payload_truncations: u64,
    pub policy_lookups: u64,
    pub policy_misses: u64,
}

impl ClassificationTelemetry {
    /// Total events processed.
    #[must_use]
    pub fn total_events(&self) -> u64 {
        self.events_accepted
            + self.events_accepted_redacted
            + self.events_rejected
            + self.events_quarantined
    }

    /// Redaction rate as a fraction (0.0 - 1.0).
    #[must_use]
    pub fn redaction_rate(&self) -> f64 {
        if self.fields_classified == 0 {
            return 0.0;
        }
        (self.fields_redacted + self.fields_removed) as f64 / self.fields_classified as f64
    }
}

// =============================================================================
// Classifier configuration
// =============================================================================

/// Configuration for the classifier engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifierConfig {
    /// Maximum audit log entries to retain.
    pub max_audit_entries: usize,
    /// Default redaction marker string.
    pub redaction_marker: String,
    /// Hash salt for deterministic hashing strategy.
    pub hash_salt: String,
    /// Whether to emit detailed per-field audit entries.
    pub detailed_audit: bool,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            max_audit_entries: 10_000,
            redaction_marker: "[CLASSIFIED]".to_string(),
            hash_salt: "ft-dc-salt".to_string(),
            detailed_audit: false,
        }
    }
}

// =============================================================================
// Classifier errors
// =============================================================================

/// Errors from classification operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassificationError {
    /// No policy found for the connector.
    NoPolicyFound { connector_id: String },
    /// Payload exceeds maximum size.
    PayloadTooLarge { size: usize, max: usize },
    /// Event was rejected by policy.
    Rejected { reason: String },
    /// Internal classification error.
    Internal { reason: String },
}

impl std::fmt::Display for ClassificationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPolicyFound { connector_id } => {
                write!(f, "no classification policy for connector '{connector_id}'")
            }
            Self::PayloadTooLarge { size, max } => {
                write!(f, "payload {size} bytes exceeds max {max}")
            }
            Self::Rejected { reason } => write!(f, "event rejected: {reason}"),
            Self::Internal { reason } => write!(f, "classification error: {reason}"),
        }
    }
}

// =============================================================================
// Main classifier engine
// =============================================================================

/// The data classification engine. Classifies, redacts, and filters connector
/// events before they enter logs, search, or policy stores.
pub struct ConnectorDataClassifier {
    config: ClassifierConfig,
    /// Per-connector policies (connector_id/pattern -> policy).
    policies: Vec<ClassificationPolicy>,
    /// Secret-pattern scanner.
    redactor: Redactor,
    /// Audit log (bounded ring buffer).
    audit_log: VecDeque<ClassificationAuditEntry>,
    /// Telemetry counters.
    telemetry: ClassificationTelemetry,
    /// Token counter for tokenize strategy.
    next_token_id: u64,
}

impl ConnectorDataClassifier {
    /// Create a new classifier with the given configuration.
    #[must_use]
    pub fn new(config: ClassifierConfig) -> Self {
        Self {
            config,
            policies: Vec::new(),
            redactor: Redactor::new(),
            audit_log: VecDeque::new(),
            telemetry: ClassificationTelemetry::default(),
            next_token_id: 1,
        }
    }

    /// Register a classification policy for a connector pattern.
    pub fn register_policy(&mut self, policy: ClassificationPolicy) {
        // Insert sorted by specificity (exact matches before wildcards).
        let is_wildcard = policy.connector_pattern.contains('*');
        let pos = self
            .policies
            .iter()
            .position(|p| p.connector_pattern.contains('*') && !is_wildcard)
            .unwrap_or(self.policies.len());
        self.policies.insert(pos, policy);
    }

    /// Find the applicable policy for a connector.
    #[must_use]
    pub fn find_policy(&self, connector_id: &str) -> Option<&ClassificationPolicy> {
        self.policies
            .iter()
            .find(|p| p.matches_connector(connector_id))
    }

    /// Classify a connector event's fields without modifying it.
    pub fn classify_event(
        &mut self,
        event: &CanonicalConnectorEvent,
    ) -> Result<ClassifiedEvent, ClassificationError> {
        self.telemetry.policy_lookups += 1;

        let policy = self
            .policies
            .iter()
            .find(|p| p.matches_connector(&event.connector_id))
            .cloned()
            .ok_or_else(|| {
                self.telemetry.policy_misses += 1;
                ClassificationError::NoPolicyFound {
                    connector_id: event.connector_id.clone(),
                }
            })?;

        // Check payload size
        let payload_str = event.payload.to_string();
        if payload_str.len() > policy.max_payload_bytes {
            self.telemetry.payload_truncations += 1;
            // We still classify, but note the truncation
        }

        let mut field_classifications = Vec::new();
        let mut overall_sensitivity = DataSensitivity::Public;
        let mut secrets_detected = false;

        // Classify top-level structural fields
        let structural_fields = [
            ("event_type", event.event_type.as_str()),
            ("event_id", event.event_id.as_str()),
            ("connector_id", event.connector_id.as_str()),
            ("correlation_id", event.correlation_id.as_str()),
        ];

        for (field_name, field_value) in &structural_fields {
            let fc = self.classify_field(&policy, field_name, field_value);
            if fc.sensitivity > overall_sensitivity {
                overall_sensitivity = fc.sensitivity;
            }
            self.telemetry.fields_classified += 1;
            field_classifications.push(fc);
        }

        // Classify metadata fields
        for (key, value) in &event.metadata {
            let path = format!("metadata.{key}");
            let fc = self.classify_field(&policy, &path, value);
            if fc.sensitivity > overall_sensitivity {
                overall_sensitivity = fc.sensitivity;
            }
            self.telemetry.fields_classified += 1;
            field_classifications.push(fc);
        }

        // Capture before mutable borrow of self
        let scan_for_secrets = policy.scan_for_secrets;

        // Classify payload fields (flatten one level of JSON objects)
        self.classify_payload_fields(
            &policy,
            "payload",
            &event.payload,
            &mut field_classifications,
            &mut overall_sensitivity,
        );

        // Secret scanning on string values
        if scan_for_secrets {
            for fc in &mut field_classifications {
                // Only scan fields not already classified as restricted+
                if fc.sensitivity < DataSensitivity::Restricted {
                    // Find the value to scan from the event
                    if let Some(val) = self.extract_field_value(event, &fc.field_path) {
                        if self.redactor.contains_secrets(&val) {
                            fc.sensitivity = DataSensitivity::Restricted;
                            fc.matched_rule = "secret-scan".to_string();
                            fc.strategy = RedactionStrategy::Mask;
                            secrets_detected = true;
                            if fc.sensitivity > overall_sensitivity {
                                overall_sensitivity = fc.sensitivity;
                            }
                        }
                    }
                }
            }
            if secrets_detected {
                self.telemetry.secrets_detected += 1;
            }
        }

        self.telemetry.events_classified += 1;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Ok(ClassifiedEvent {
            event_id: event.event_id.clone(),
            connector_id: event.connector_id.clone(),
            overall_sensitivity,
            field_classifications,
            policy_id: policy.policy_id.clone(),
            secrets_detected,
            classified_at_ms: now_ms,
        })
    }

    /// Classify a single field against the policy rules.
    /// Rules are matched against both the full path (e.g., "payload.password")
    /// and the leaf field name (e.g., "password").
    #[allow(clippy::unused_self)]
    fn classify_field(
        &self,
        policy: &ClassificationPolicy,
        field_name: &str,
        field_value: &str,
    ) -> FieldClassification {
        // Extract the leaf field name for matching
        let leaf_name = field_name.rsplit('.').next().unwrap_or(field_name);

        // Sort rules by priority descending
        let mut sorted_rules: Vec<&ClassificationRule> =
            policy.rules.iter().filter(|r| r.enabled).collect();
        sorted_rules.sort_by_key(|r| std::cmp::Reverse(r.priority));

        for rule in sorted_rules {
            let field_match = rule.matches_field(field_name) || rule.matches_field(leaf_name);
            if field_match && rule.matches_content(field_value) {
                return FieldClassification {
                    field_path: field_name.to_string(),
                    sensitivity: rule.sensitivity,
                    matched_rule: rule.rule_id.clone(),
                    strategy: rule.effective_strategy(),
                };
            }
        }

        // Default classification
        FieldClassification {
            field_path: field_name.to_string(),
            sensitivity: policy.default_sensitivity,
            matched_rule: "default".to_string(),
            strategy: RedactionStrategy::for_sensitivity(policy.default_sensitivity),
        }
    }

    /// Recursively classify payload JSON fields (up to 2 levels deep).
    fn classify_payload_fields(
        &mut self,
        policy: &ClassificationPolicy,
        prefix: &str,
        value: &serde_json::Value,
        classifications: &mut Vec<FieldClassification>,
        overall: &mut DataSensitivity,
    ) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, val) in map {
                    let path = format!("{prefix}.{key}");
                    match val {
                        serde_json::Value::Object(inner_map) => {
                            // Go one more level
                            for (ik, iv) in inner_map {
                                let inner_path = format!("{path}.{ik}");
                                let val_str = json_value_to_string(iv);
                                let fc = self.classify_field(policy, &inner_path, &val_str);
                                if fc.sensitivity > *overall {
                                    *overall = fc.sensitivity;
                                }
                                self.telemetry.fields_classified += 1;
                                classifications.push(fc);
                            }
                        }
                        _ => {
                            let val_str = json_value_to_string(val);
                            let fc = self.classify_field(policy, &path, &val_str);
                            if fc.sensitivity > *overall {
                                *overall = fc.sensitivity;
                            }
                            self.telemetry.fields_classified += 1;
                            classifications.push(fc);
                        }
                    }
                }
            }
            serde_json::Value::String(s) => {
                let fc = self.classify_field(policy, prefix, s);
                if fc.sensitivity > *overall {
                    *overall = fc.sensitivity;
                }
                self.telemetry.fields_classified += 1;
                classifications.push(fc);
            }
            _ => {
                // Scalar values (number, bool, null) — always public
                classifications.push(FieldClassification {
                    field_path: prefix.to_string(),
                    sensitivity: DataSensitivity::Public,
                    matched_rule: "scalar".to_string(),
                    strategy: RedactionStrategy::Passthrough,
                });
                self.telemetry.fields_classified += 1;
            }
        }
    }

    /// Extract a field value from an event by path (best-effort).
    #[allow(clippy::unused_self)]
    fn extract_field_value(&self, event: &CanonicalConnectorEvent, path: &str) -> Option<String> {
        if let Some(rest) = path.strip_prefix("metadata.") {
            return event.metadata.get(rest).cloned();
        }
        if let Some(rest) = path.strip_prefix("payload.") {
            return extract_json_path(&event.payload, rest).map(|v| json_value_to_string(&v));
        }
        match path {
            "event_type" => Some(event.event_type.clone()),
            "event_id" => Some(event.event_id.clone()),
            "connector_id" => Some(event.connector_id.clone()),
            "correlation_id" => Some(event.correlation_id.clone()),
            _ => None,
        }
    }

    /// Determine the ingestion decision for a classified event.
    #[must_use]
    pub fn ingestion_decision(
        &self,
        classified: &ClassifiedEvent,
        policy: &ClassificationPolicy,
    ) -> IngestionDecision {
        if classified.has_prohibited() && !policy.allow_prohibited {
            return IngestionDecision::Reject {
                reason: "event contains prohibited-level data".to_string(),
            };
        }

        if classified.requires_redaction() {
            return IngestionDecision::AcceptRedacted;
        }

        IngestionDecision::Accept
    }

    /// Apply redaction to a classified event, producing a safe-to-store version.
    pub fn redact_event(
        &mut self,
        event: &CanonicalConnectorEvent,
        classified: &ClassifiedEvent,
    ) -> RedactedEvent {
        let decision = if classified.has_prohibited() {
            IngestionDecision::Reject {
                reason: "prohibited data".into(),
            }
        } else if classified.requires_redaction() {
            IngestionDecision::AcceptRedacted
        } else {
            IngestionDecision::Accept
        };

        self.redact_event_with_decision(event, classified, decision)
    }

    /// Apply redaction while recording an explicit ingestion decision.
    pub fn redact_event_with_decision(
        &mut self,
        event: &CanonicalConnectorEvent,
        classified: &ClassifiedEvent,
        decision: IngestionDecision,
    ) -> RedactedEvent {
        let mut redacted_event = event.clone();
        let mut actions = Vec::new();

        // Redact metadata fields
        for fc in &classified.field_classifications {
            if !fc.sensitivity.requires_redaction() {
                continue;
            }

            if let Some(rest) = fc.field_path.strip_prefix("metadata.") {
                if let Some(original) = redacted_event.metadata.get(rest) {
                    let original_bytes = original.len();
                    let new_val = self.apply_strategy(&fc.strategy, original);
                    if let Some(new_val) = new_val {
                        redacted_event.metadata.insert(rest.to_string(), new_val);
                        self.telemetry.fields_redacted += 1;
                    } else {
                        redacted_event.metadata.remove(rest);
                        self.telemetry.fields_removed += 1;
                    }
                    actions.push(RedactionAction {
                        field_path: fc.field_path.clone(),
                        strategy: fc.strategy.clone(),
                        sensitivity: fc.sensitivity,
                        original_bytes,
                    });
                }
            }
        }

        // Redact payload fields
        let mut payload = redacted_event.payload.clone();
        for fc in &classified.field_classifications {
            if !fc.sensitivity.requires_redaction() {
                continue;
            }
            if let Some(rest) = fc.field_path.strip_prefix("payload.") {
                if let Some(original_val) = extract_json_path(&event.payload, rest) {
                    let original_str = json_value_to_string(&original_val);
                    let original_bytes = original_str.len();
                    let new_val = self.apply_strategy(&fc.strategy, &original_str);
                    set_json_path(&mut payload, rest, new_val);
                    if fc.strategy == RedactionStrategy::Remove {
                        self.telemetry.fields_removed += 1;
                    } else {
                        self.telemetry.fields_redacted += 1;
                    }
                    actions.push(RedactionAction {
                        field_path: fc.field_path.clone(),
                        strategy: fc.strategy.clone(),
                        sensitivity: fc.sensitivity,
                        original_bytes,
                    });
                }
            }
        }
        redacted_event.payload = payload;

        // Record audit entry
        let fields_redacted = actions
            .iter()
            .filter(|a| !matches!(a.strategy, RedactionStrategy::Remove))
            .count() as u32;
        let fields_removed = actions
            .iter()
            .filter(|a| matches!(a.strategy, RedactionStrategy::Remove))
            .count() as u32;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let audit = ClassificationAuditEntry {
            event_id: classified.event_id.clone(),
            connector_id: classified.connector_id.clone(),
            policy_id: classified.policy_id.clone(),
            sensitivity: classified.overall_sensitivity,
            decision: decision.clone(),
            fields_redacted,
            fields_removed,
            secrets_detected: classified.secrets_detected,
            timestamp_ms: now_ms,
        };
        self.push_audit(audit);

        match decision {
            IngestionDecision::Accept => {
                self.telemetry.events_accepted += 1;
            }
            IngestionDecision::AcceptRedacted => {
                self.telemetry.events_accepted_redacted += 1;
            }
            IngestionDecision::Reject { .. } => {
                self.telemetry.events_rejected += 1;
            }
            IngestionDecision::Quarantine { .. } => {
                self.telemetry.events_quarantined += 1;
            }
        }

        RedactedEvent {
            event: redacted_event,
            classification: classified.clone(),
            redaction_actions: actions,
        }
    }

    /// Apply a redaction strategy to a string value.
    fn apply_strategy(&mut self, strategy: &RedactionStrategy, value: &str) -> Option<String> {
        match strategy {
            RedactionStrategy::Mask => Some(self.config.redaction_marker.clone()),
            RedactionStrategy::Hash => {
                let hash = simple_hash(&self.config.hash_salt, value);
                Some(format!("hash:{hash:016x}"))
            }
            RedactionStrategy::Truncate { max_len } => {
                if value.len() <= *max_len {
                    Some(value.to_string())
                } else {
                    let truncated = &value[..value.len().min(*max_len)];
                    Some(format!("{truncated}[...]"))
                }
            }
            RedactionStrategy::Remove => None,
            RedactionStrategy::Tokenize { token_prefix } => {
                let id = self.next_token_id;
                self.next_token_id += 1;
                Some(format!("{token_prefix}{id}"))
            }
            RedactionStrategy::Passthrough => Some(value.to_string()),
        }
    }

    /// Push an audit entry, evicting old ones if at capacity.
    fn push_audit(&mut self, entry: ClassificationAuditEntry) {
        if self.audit_log.len() >= self.config.max_audit_entries {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(entry);
    }

    /// Get the audit log.
    #[must_use]
    pub fn audit_log(&self) -> &VecDeque<ClassificationAuditEntry> {
        &self.audit_log
    }

    /// Get telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> &ClassificationTelemetry {
        &self.telemetry
    }

    /// Get registered policy count.
    #[must_use]
    pub fn policy_count(&self) -> usize {
        self.policies.len()
    }

    /// Serialize audit log to JSON.
    pub fn audit_log_json(&self) -> Result<String, serde_json::Error> {
        let entries: Vec<&ClassificationAuditEntry> = self.audit_log.iter().collect();
        serde_json::to_string_pretty(&entries)
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Convert a JSON value to a string for classification.
fn json_value_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Extract a value from a JSON object by dot-separated path (1-2 levels).
fn extract_json_path(value: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = path.splitn(2, '.').collect();
    match parts.len() {
        1 => value.get(parts[0]).cloned(),
        2 => value.get(parts[0]).and_then(|v| v.get(parts[1]).cloned()),
        _ => None,
    }
}

/// Set a value in a JSON object by dot-separated path (1-2 levels).
/// If new_value is None, removes the field.
fn set_json_path(value: &mut serde_json::Value, path: &str, new_value: Option<String>) {
    let parts: Vec<&str> = path.splitn(2, '.').collect();
    match parts.len() {
        1 => {
            if let Some(obj) = value.as_object_mut() {
                if let Some(val) = new_value {
                    obj.insert(parts[0].to_string(), serde_json::Value::String(val));
                } else {
                    obj.remove(parts[0]);
                }
            }
        }
        2 => {
            if let Some(obj) = value.as_object_mut() {
                if let Some(inner) = obj.get_mut(parts[0]) {
                    if let Some(inner_obj) = inner.as_object_mut() {
                        if let Some(val) = new_value {
                            inner_obj.insert(parts[1].to_string(), serde_json::Value::String(val));
                        } else {
                            inner_obj.remove(parts[1]);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

/// Simple non-cryptographic hash for deterministic tokenization.
/// Uses FNV-1a variant.
fn simple_hash(salt: &str, value: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in salt.bytes().chain(value.bytes()) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_event_model::{CanonicalConnectorEvent, EventDirection};

    // ── Helpers ──

    fn test_event(connector_id: &str) -> CanonicalConnectorEvent {
        let mut event =
            CanonicalConnectorEvent::new(EventDirection::Inbound, connector_id, "test.event", {
                serde_json::json!({
                    "message": "hello world",
                    "count": 42,
                    "credentials": {
                        "token": "sk-secret-12345",
                        "provider": "github"
                    }
                })
            });
        event.event_id = "evt-001".to_string();
        event.correlation_id = "corr-001".to_string();
        event
    }

    fn test_classifier() -> ConnectorDataClassifier {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        classifier.register_policy(ClassificationPolicy::default());
        classifier
    }

    // ── DataSensitivity ──

    #[test]
    fn sensitivity_ordering() {
        assert!(DataSensitivity::Public < DataSensitivity::Internal);
        assert!(DataSensitivity::Internal < DataSensitivity::Confidential);
        assert!(DataSensitivity::Confidential < DataSensitivity::Restricted);
        assert!(DataSensitivity::Restricted < DataSensitivity::Prohibited);
    }

    #[test]
    fn sensitivity_requires_redaction() {
        assert!(!DataSensitivity::Public.requires_redaction());
        assert!(!DataSensitivity::Internal.requires_redaction());
        assert!(DataSensitivity::Confidential.requires_redaction());
        assert!(DataSensitivity::Restricted.requires_redaction());
        assert!(DataSensitivity::Prohibited.requires_redaction());
    }

    #[test]
    fn sensitivity_must_remove() {
        assert!(!DataSensitivity::Restricted.must_remove());
        assert!(DataSensitivity::Prohibited.must_remove());
    }

    #[test]
    fn sensitivity_display() {
        assert_eq!(DataSensitivity::Public.to_string(), "public");
        assert_eq!(DataSensitivity::Prohibited.to_string(), "prohibited");
    }

    #[test]
    fn sensitivity_serde_roundtrip() {
        for s in [
            DataSensitivity::Public,
            DataSensitivity::Internal,
            DataSensitivity::Confidential,
            DataSensitivity::Restricted,
            DataSensitivity::Prohibited,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let rt: DataSensitivity = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }

    // ── RedactionStrategy ──

    #[test]
    fn strategy_for_sensitivity() {
        assert!(matches!(
            RedactionStrategy::for_sensitivity(DataSensitivity::Public),
            RedactionStrategy::Passthrough
        ));
        assert!(matches!(
            RedactionStrategy::for_sensitivity(DataSensitivity::Confidential),
            RedactionStrategy::Truncate { .. }
        ));
        assert!(matches!(
            RedactionStrategy::for_sensitivity(DataSensitivity::Restricted),
            RedactionStrategy::Mask
        ));
        assert!(matches!(
            RedactionStrategy::for_sensitivity(DataSensitivity::Prohibited),
            RedactionStrategy::Remove
        ));
    }

    #[test]
    fn strategy_display() {
        assert_eq!(RedactionStrategy::Mask.to_string(), "mask");
        assert_eq!(RedactionStrategy::Hash.to_string(), "hash");
        assert_eq!(
            RedactionStrategy::Truncate { max_len: 64 }.to_string(),
            "truncate(64)"
        );
        assert_eq!(RedactionStrategy::Remove.to_string(), "remove");
        assert_eq!(
            RedactionStrategy::Tokenize {
                token_prefix: "tok-".into()
            }
            .to_string(),
            "tokenize(tok-)"
        );
    }

    #[test]
    fn strategy_serde_roundtrip() {
        let strategies = vec![
            RedactionStrategy::Mask,
            RedactionStrategy::Hash,
            RedactionStrategy::Truncate { max_len: 100 },
            RedactionStrategy::Remove,
            RedactionStrategy::Tokenize {
                token_prefix: "t-".into(),
            },
            RedactionStrategy::Passthrough,
        ];
        for s in strategies {
            let json = serde_json::to_string(&s).unwrap();
            let rt: RedactionStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }

    // ── ClassificationRule ──

    #[test]
    fn rule_matches_exact_field() {
        let rule =
            ClassificationRule::new("r1", DataSensitivity::Restricted, vec!["password".into()]);
        assert!(rule.matches_field("password"));
        assert!(!rule.matches_field("password_hash"));
    }

    #[test]
    fn rule_matches_prefix_pattern() {
        let rule = ClassificationRule::new(
            "r1",
            DataSensitivity::Restricted,
            vec!["credential*".into()],
        );
        assert!(rule.matches_field("credential"));
        assert!(rule.matches_field("credentials"));
        assert!(rule.matches_field("credential_id"));
        assert!(!rule.matches_field("cred"));
    }

    #[test]
    fn rule_matches_content() {
        let mut rule = ClassificationRule::new("r1", DataSensitivity::Restricted, vec!["*".into()]);
        rule.content_patterns = vec!["sk-ant-".into()];
        assert!(rule.matches_content("token=sk-ant-abc123"));
        assert!(!rule.matches_content("safe text"));
    }

    #[test]
    fn rule_no_content_patterns_matches_all() {
        let rule =
            ClassificationRule::new("r1", DataSensitivity::Public, vec!["event_type".into()]);
        assert!(rule.matches_content("anything"));
    }

    #[test]
    fn rule_effective_strategy_override() {
        let mut rule =
            ClassificationRule::new("r1", DataSensitivity::Restricted, vec!["field".into()]);
        rule.redaction_override = Some(RedactionStrategy::Hash);
        assert!(matches!(rule.effective_strategy(), RedactionStrategy::Hash));
    }

    #[test]
    fn rule_effective_strategy_default() {
        let rule = ClassificationRule::new("r1", DataSensitivity::Restricted, vec!["field".into()]);
        assert!(matches!(rule.effective_strategy(), RedactionStrategy::Mask));
    }

    // ── ClassificationPolicy ──

    #[test]
    fn default_policy_has_builtin_rules() {
        let policy = ClassificationPolicy::default();
        assert!(!policy.rules.is_empty());
        assert!(policy.scan_for_secrets);
    }

    #[test]
    fn policy_matches_wildcard() {
        let policy = ClassificationPolicy::default();
        assert!(policy.matches_connector("any-connector"));
        assert!(policy.matches_connector("github"));
    }

    #[test]
    fn policy_matches_exact() {
        let mut policy = ClassificationPolicy::default();
        policy.connector_pattern = "github".to_string();
        assert!(policy.matches_connector("github"));
        assert!(!policy.matches_connector("gitlab"));
    }

    #[test]
    fn policy_matches_prefix_wildcard() {
        let mut policy = ClassificationPolicy::default();
        policy.connector_pattern = "github-*".to_string();
        assert!(policy.matches_connector("github-actions"));
        assert!(policy.matches_connector("github-webhooks"));
        assert!(!policy.matches_connector("gitlab"));
    }

    // ── ClassifiedEvent ──

    #[test]
    fn classified_event_sensitivity_histogram() {
        let classified = ClassifiedEvent {
            event_id: "e1".into(),
            connector_id: "c1".into(),
            overall_sensitivity: DataSensitivity::Restricted,
            field_classifications: vec![
                FieldClassification {
                    field_path: "f1".into(),
                    sensitivity: DataSensitivity::Public,
                    matched_rule: "r1".into(),
                    strategy: RedactionStrategy::Passthrough,
                },
                FieldClassification {
                    field_path: "f2".into(),
                    sensitivity: DataSensitivity::Public,
                    matched_rule: "r1".into(),
                    strategy: RedactionStrategy::Passthrough,
                },
                FieldClassification {
                    field_path: "f3".into(),
                    sensitivity: DataSensitivity::Restricted,
                    matched_rule: "r2".into(),
                    strategy: RedactionStrategy::Mask,
                },
            ],
            policy_id: "p1".into(),
            secrets_detected: false,
            classified_at_ms: 0,
        };

        let hist = classified.sensitivity_histogram();
        assert_eq!(hist.get(&DataSensitivity::Public), Some(&2));
        assert_eq!(hist.get(&DataSensitivity::Restricted), Some(&1));
    }

    #[test]
    fn classified_event_has_prohibited() {
        let classified = ClassifiedEvent {
            event_id: "e1".into(),
            connector_id: "c1".into(),
            overall_sensitivity: DataSensitivity::Prohibited,
            field_classifications: vec![FieldClassification {
                field_path: "password".into(),
                sensitivity: DataSensitivity::Prohibited,
                matched_rule: "builtin-secrets".into(),
                strategy: RedactionStrategy::Remove,
            }],
            policy_id: "default".into(),
            secrets_detected: false,
            classified_at_ms: 0,
        };
        assert!(classified.has_prohibited());
    }

    // ── IngestionDecision ──

    #[test]
    fn ingestion_decision_display() {
        assert_eq!(IngestionDecision::Accept.to_string(), "accept");
        assert_eq!(
            IngestionDecision::AcceptRedacted.to_string(),
            "accept_redacted"
        );
        assert_eq!(
            IngestionDecision::Reject {
                reason: "test".into()
            }
            .to_string(),
            "reject: test"
        );
    }

    #[test]
    fn ingestion_decision_predicates() {
        assert!(IngestionDecision::Accept.is_accepted());
        assert!(IngestionDecision::AcceptRedacted.is_accepted());
        assert!(!IngestionDecision::Reject { reason: "x".into() }.is_accepted());
        assert!(IngestionDecision::Reject { reason: "x".into() }.is_rejected());
        assert!(!IngestionDecision::Accept.is_rejected());
    }

    #[test]
    fn ingestion_decision_serde_roundtrip() {
        let decisions = vec![
            IngestionDecision::Accept,
            IngestionDecision::AcceptRedacted,
            IngestionDecision::Reject {
                reason: "test".into(),
            },
            IngestionDecision::Quarantine {
                reason: "review".into(),
            },
        ];
        for d in decisions {
            let json = serde_json::to_string(&d).unwrap();
            let rt: IngestionDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(d, rt);
        }
    }

    // ── ClassificationError ──

    #[test]
    fn error_display() {
        let e = ClassificationError::NoPolicyFound {
            connector_id: "foo".into(),
        };
        assert!(e.to_string().contains("foo"));

        let e = ClassificationError::PayloadTooLarge {
            size: 2000,
            max: 1000,
        };
        assert!(e.to_string().contains("2000"));

        let e = ClassificationError::Rejected {
            reason: "bad".into(),
        };
        assert!(e.to_string().contains("bad"));
    }

    // ── Classifier engine ──

    #[test]
    fn classify_event_with_default_policy() {
        let mut classifier = test_classifier();
        let event = test_event("test-connector");
        let classified = classifier.classify_event(&event).unwrap();

        assert_eq!(classified.event_id, "evt-001");
        assert_eq!(classified.connector_id, "test-connector");
        assert_eq!(classified.policy_id, "default");
        assert!(!classified.field_classifications.is_empty());
    }

    #[test]
    fn classify_event_detects_prohibited_fields() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "password": "super-secret-pass",
            "status": "ok"
        });

        let classified = classifier.classify_event(&event).unwrap();
        assert!(classified.has_prohibited());
        assert!(classified.overall_sensitivity >= DataSensitivity::Prohibited);
    }

    #[test]
    fn classify_event_no_policy_returns_error() {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        // No policies registered
        let event = test_event("test-connector");
        let result = classifier.classify_event(&event);
        assert!(result.is_err());
        let check = matches!(
            result.unwrap_err(),
            ClassificationError::NoPolicyFound { .. }
        );
        assert!(check);
    }

    #[test]
    fn classify_structural_fields_as_public() {
        let mut classifier = test_classifier();
        let event = test_event("test-connector");
        let classified = classifier.classify_event(&event).unwrap();

        let event_type_fc = classified
            .field_classifications
            .iter()
            .find(|fc| fc.field_path == "event_type")
            .unwrap();
        assert_eq!(event_type_fc.sensitivity, DataSensitivity::Public);
    }

    #[test]
    fn classify_metadata_fields() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event
            .metadata
            .insert("password".to_string(), "secret123".to_string());

        let classified = classifier.classify_event(&event).unwrap();
        let pw_fc = classified
            .field_classifications
            .iter()
            .find(|fc| fc.field_path == "metadata.password")
            .unwrap();
        assert_eq!(pw_fc.sensitivity, DataSensitivity::Prohibited);
    }

    // ── Redaction ──

    #[test]
    fn redact_event_masks_restricted_fields() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event
            .metadata
            .insert("auth_token".to_string(), "bearer-xyz-secret".to_string());

        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event(&event, &classified);

        // auth_token metadata should be redacted (auth* matches restricted rule)
        let meta_val = redacted.event.metadata.get("auth_token");
        if let Some(val) = meta_val {
            assert!(
                val == "[CLASSIFIED]" || !val.contains("bearer-xyz-secret"),
                "auth_token should be redacted"
            );
        }
    }

    #[test]
    fn redact_event_removes_prohibited_payload_fields() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "password": "super-secret",
            "status": "ok"
        });

        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event(&event, &classified);

        // password field should be removed from payload
        assert!(redacted.event.payload.get("password").is_none());
        // status should be preserved
        assert!(redacted.event.payload.get("status").is_some());
    }

    #[test]
    fn redact_event_records_actions() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "password": "secret",
            "status": "ok"
        });

        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event(&event, &classified);

        assert!(
            !redacted.redaction_actions.is_empty(),
            "should have redaction actions"
        );

        let pw_action = redacted
            .redaction_actions
            .iter()
            .find(|a| a.field_path.contains("password"));
        assert!(pw_action.is_some());
    }

    #[test]
    fn redact_event_updates_telemetry() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "password": "secret"
        });

        let classified = classifier.classify_event(&event).unwrap();
        let _redacted = classifier.redact_event(&event, &classified);

        let telem = classifier.telemetry();
        assert!(telem.fields_removed > 0 || telem.fields_redacted > 0);
    }

    // ── Ingestion decisions ──

    #[test]
    fn ingestion_rejects_prohibited() {
        let classifier = test_classifier();
        let classified = ClassifiedEvent {
            event_id: "e1".into(),
            connector_id: "c1".into(),
            overall_sensitivity: DataSensitivity::Prohibited,
            field_classifications: vec![FieldClassification {
                field_path: "password".into(),
                sensitivity: DataSensitivity::Prohibited,
                matched_rule: "builtin-secrets".into(),
                strategy: RedactionStrategy::Remove,
            }],
            policy_id: "default".into(),
            secrets_detected: false,
            classified_at_ms: 0,
        };
        let policy = ClassificationPolicy::default();
        let decision = classifier.ingestion_decision(&classified, &policy);
        assert!(decision.is_rejected());
    }

    #[test]
    fn ingestion_accepts_public_events() {
        let classifier = test_classifier();
        let classified = ClassifiedEvent {
            event_id: "e1".into(),
            connector_id: "c1".into(),
            overall_sensitivity: DataSensitivity::Public,
            field_classifications: vec![FieldClassification {
                field_path: "event_type".into(),
                sensitivity: DataSensitivity::Public,
                matched_rule: "builtin-structural".into(),
                strategy: RedactionStrategy::Passthrough,
            }],
            policy_id: "default".into(),
            secrets_detected: false,
            classified_at_ms: 0,
        };
        let policy = ClassificationPolicy::default();
        let decision = classifier.ingestion_decision(&classified, &policy);
        assert_eq!(decision, IngestionDecision::Accept);
    }

    #[test]
    fn ingestion_accepts_redacted_for_confidential() {
        let classifier = test_classifier();
        let classified = ClassifiedEvent {
            event_id: "e1".into(),
            connector_id: "c1".into(),
            overall_sensitivity: DataSensitivity::Confidential,
            field_classifications: vec![FieldClassification {
                field_path: "message".into(),
                sensitivity: DataSensitivity::Confidential,
                matched_rule: "builtin-user-content".into(),
                strategy: RedactionStrategy::Truncate { max_len: 64 },
            }],
            policy_id: "default".into(),
            secrets_detected: false,
            classified_at_ms: 0,
        };
        let policy = ClassificationPolicy::default();
        let decision = classifier.ingestion_decision(&classified, &policy);
        assert_eq!(decision, IngestionDecision::AcceptRedacted);
    }

    #[test]
    fn ingestion_allows_prohibited_with_override() {
        let classifier = test_classifier();
        let classified = ClassifiedEvent {
            event_id: "e1".into(),
            connector_id: "c1".into(),
            overall_sensitivity: DataSensitivity::Prohibited,
            field_classifications: vec![FieldClassification {
                field_path: "password".into(),
                sensitivity: DataSensitivity::Prohibited,
                matched_rule: "builtin-secrets".into(),
                strategy: RedactionStrategy::Remove,
            }],
            policy_id: "default".into(),
            secrets_detected: false,
            classified_at_ms: 0,
        };
        let mut policy = ClassificationPolicy::default();
        policy.allow_prohibited = true;
        let decision = classifier.ingestion_decision(&classified, &policy);
        // With allow_prohibited, it should accept with redaction (not reject)
        assert_eq!(decision, IngestionDecision::AcceptRedacted);
    }

    // ── Apply strategy ──

    #[test]
    fn apply_mask_strategy() {
        let mut classifier = test_classifier();
        let result = classifier.apply_strategy(&RedactionStrategy::Mask, "secret");
        assert_eq!(result, Some("[CLASSIFIED]".to_string()));
    }

    #[test]
    fn apply_hash_strategy_deterministic() {
        let mut classifier = test_classifier();
        let r1 = classifier.apply_strategy(&RedactionStrategy::Hash, "value1");
        let r2 = classifier.apply_strategy(&RedactionStrategy::Hash, "value1");
        assert_eq!(r1, r2, "hash should be deterministic");

        let r3 = classifier.apply_strategy(&RedactionStrategy::Hash, "value2");
        assert_ne!(r1, r3, "different inputs should produce different hashes");
    }

    #[test]
    fn apply_truncate_strategy() {
        let mut classifier = test_classifier();
        let result =
            classifier.apply_strategy(&RedactionStrategy::Truncate { max_len: 5 }, "hello world");
        assert_eq!(result, Some("hello[...]".to_string()));
    }

    #[test]
    fn apply_truncate_short_value() {
        let mut classifier = test_classifier();
        let result =
            classifier.apply_strategy(&RedactionStrategy::Truncate { max_len: 100 }, "short");
        assert_eq!(result, Some("short".to_string()));
    }

    #[test]
    fn apply_remove_strategy() {
        let mut classifier = test_classifier();
        let result = classifier.apply_strategy(&RedactionStrategy::Remove, "anything");
        assert!(result.is_none());
    }

    #[test]
    fn apply_tokenize_strategy() {
        let mut classifier = test_classifier();
        let r1 = classifier.apply_strategy(
            &RedactionStrategy::Tokenize {
                token_prefix: "tok-".into(),
            },
            "value1",
        );
        let r2 = classifier.apply_strategy(
            &RedactionStrategy::Tokenize {
                token_prefix: "tok-".into(),
            },
            "value2",
        );
        assert_eq!(r1, Some("tok-1".to_string()));
        assert_eq!(r2, Some("tok-2".to_string()));
    }

    #[test]
    fn apply_passthrough_strategy() {
        let mut classifier = test_classifier();
        let result = classifier.apply_strategy(&RedactionStrategy::Passthrough, "hello");
        assert_eq!(result, Some("hello".to_string()));
    }

    // ── Audit log ──

    #[test]
    fn audit_log_grows_with_redactions() {
        let mut classifier = test_classifier();
        assert!(classifier.audit_log().is_empty());

        let event = test_event("test-connector");
        let classified = classifier.classify_event(&event).unwrap();
        classifier.redact_event(&event, &classified);

        assert_eq!(classifier.audit_log().len(), 1);
    }

    #[test]
    fn audit_log_bounded() {
        let config = ClassifierConfig {
            max_audit_entries: 3,
            ..Default::default()
        };
        let mut classifier = ConnectorDataClassifier::new(config);
        classifier.register_policy(ClassificationPolicy::default());

        for i in 0..5 {
            let mut event = test_event("test-connector");
            event.event_id = format!("evt-{i}");
            let classified = classifier.classify_event(&event).unwrap();
            classifier.redact_event(&event, &classified);
        }

        assert_eq!(classifier.audit_log().len(), 3);
        // Oldest should have been evicted
        assert_eq!(classifier.audit_log().front().unwrap().event_id, "evt-2");
    }

    #[test]
    fn audit_log_json_serializes() {
        let mut classifier = test_classifier();
        let event = test_event("test-connector");
        let classified = classifier.classify_event(&event).unwrap();
        classifier.redact_event(&event, &classified);

        let json = classifier.audit_log_json().unwrap();
        assert!(json.contains("evt-001"));
    }

    #[test]
    fn redact_event_with_explicit_accept_redacted_overrides_reject_audit() {
        let mut classifier = test_classifier();
        let mut event = test_event("github");
        event.payload = serde_json::json!({
            "password": "super-secret",
            "status": "ok"
        });

        let classified = classifier.classify_event(&event).unwrap();
        let redacted = classifier.redact_event_with_decision(
            &event,
            &classified,
            IngestionDecision::AcceptRedacted,
        );

        assert!(redacted.event.payload.get("password").is_none());
        let audit = classifier.audit_log().back().unwrap();
        assert_eq!(audit.decision, IngestionDecision::AcceptRedacted);
        assert_eq!(classifier.telemetry().events_accepted_redacted, 1);
        assert_eq!(classifier.telemetry().events_rejected, 0);
    }

    #[test]
    fn redact_event_with_quarantine_tracks_quarantine_telemetry() {
        let mut classifier = test_classifier();
        let event = test_event("test-connector");
        let classified = classifier.classify_event(&event).unwrap();

        let _ = classifier.redact_event_with_decision(
            &event,
            &classified,
            IngestionDecision::Quarantine {
                reason: "manual review required".to_string(),
            },
        );

        let audit = classifier.audit_log().back().unwrap();
        assert_eq!(
            audit.decision,
            IngestionDecision::Quarantine {
                reason: "manual review required".to_string(),
            }
        );
        assert_eq!(classifier.telemetry().events_quarantined, 1);
    }

    // ── Telemetry ──

    #[test]
    fn telemetry_initial_state() {
        let classifier = test_classifier();
        let t = classifier.telemetry();
        assert_eq!(t.events_classified, 0);
        assert_eq!(t.total_events(), 0);
    }

    #[test]
    fn telemetry_after_classification() {
        let mut classifier = test_classifier();
        let event = test_event("test-connector");
        let _classified = classifier.classify_event(&event).unwrap();

        let t = classifier.telemetry();
        assert_eq!(t.events_classified, 1);
        assert!(t.fields_classified > 0);
    }

    #[test]
    fn telemetry_redaction_rate() {
        let t = ClassificationTelemetry {
            fields_classified: 10,
            fields_redacted: 3,
            fields_removed: 2,
            ..Default::default()
        };
        let rate = t.redaction_rate();
        assert!((rate - 0.5).abs() < 0.001);
    }

    #[test]
    fn telemetry_redaction_rate_zero_fields() {
        let t = ClassificationTelemetry::default();
        assert_eq!(t.redaction_rate(), 0.0);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let t = ClassificationTelemetry {
            events_classified: 100,
            events_accepted: 80,
            events_accepted_redacted: 15,
            events_rejected: 5,
            fields_classified: 500,
            fields_redacted: 50,
            fields_removed: 10,
            secrets_detected: 3,
            ..Default::default()
        };
        let json = serde_json::to_string(&t).unwrap();
        let rt: ClassificationTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(t, rt);
    }

    // ── Policy registration ──

    #[test]
    fn register_specific_policy_before_wildcard() {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());

        // Register wildcard first
        classifier.register_policy(ClassificationPolicy::default());

        // Register specific policy
        let mut specific = ClassificationPolicy::default();
        specific.policy_id = "github-specific".to_string();
        specific.connector_pattern = "github".to_string();
        classifier.register_policy(specific);

        // Specific should match first
        let policy = classifier.find_policy("github").unwrap();
        assert_eq!(policy.policy_id, "github-specific");

        // Wildcard should match other connectors
        let policy = classifier.find_policy("gitlab").unwrap();
        assert_eq!(policy.policy_id, "default");
    }

    #[test]
    fn policy_count() {
        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        assert_eq!(classifier.policy_count(), 0);
        classifier.register_policy(ClassificationPolicy::default());
        assert_eq!(classifier.policy_count(), 1);
    }

    // ── JSON helpers ──

    #[test]
    fn extract_json_path_single_level() {
        let val = serde_json::json!({"key": "value"});
        assert_eq!(
            extract_json_path(&val, "key"),
            Some(serde_json::json!("value"))
        );
        assert_eq!(extract_json_path(&val, "missing"), None);
    }

    #[test]
    fn extract_json_path_two_levels() {
        let val = serde_json::json!({"outer": {"inner": 42}});
        assert_eq!(
            extract_json_path(&val, "outer.inner"),
            Some(serde_json::json!(42))
        );
    }

    #[test]
    fn set_json_path_inserts_value() {
        let mut val = serde_json::json!({"key": "old"});
        set_json_path(&mut val, "key", Some("new".to_string()));
        assert_eq!(val.get("key").unwrap().as_str().unwrap(), "new");
    }

    #[test]
    fn set_json_path_removes_value() {
        let mut val = serde_json::json!({"key": "value", "keep": "yes"});
        set_json_path(&mut val, "key", None);
        assert!(val.get("key").is_none());
        assert!(val.get("keep").is_some());
    }

    #[test]
    fn set_json_path_nested() {
        let mut val = serde_json::json!({"outer": {"inner": "old"}});
        set_json_path(&mut val, "outer.inner", Some("new".to_string()));
        assert_eq!(
            val.get("outer")
                .unwrap()
                .get("inner")
                .unwrap()
                .as_str()
                .unwrap(),
            "new"
        );
    }

    // ── Hash determinism ──

    #[test]
    fn simple_hash_deterministic() {
        let h1 = simple_hash("salt", "value");
        let h2 = simple_hash("salt", "value");
        assert_eq!(h1, h2);
    }

    #[test]
    fn simple_hash_different_inputs() {
        let h1 = simple_hash("salt", "value1");
        let h2 = simple_hash("salt", "value2");
        assert_ne!(h1, h2);
    }

    #[test]
    fn simple_hash_different_salts() {
        let h1 = simple_hash("salt1", "value");
        let h2 = simple_hash("salt2", "value");
        assert_ne!(h1, h2);
    }

    // ── ClassifierConfig ──

    #[test]
    fn default_config_reasonable() {
        let config = ClassifierConfig::default();
        assert_eq!(config.max_audit_entries, 10_000);
        assert_eq!(config.redaction_marker, "[CLASSIFIED]");
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ClassifierConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let rt: ClassifierConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(rt.max_audit_entries, config.max_audit_entries);
        assert_eq!(rt.redaction_marker, config.redaction_marker);
    }

    // ── End-to-end classify + redact + ingest ──

    #[test]
    fn e2e_classify_redact_safe_event() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "event_type": "push",
            "count": 5
        });

        let classified = classifier.classify_event(&event).unwrap();
        let policy = classifier.find_policy("test-connector").unwrap().clone();
        let decision = classifier.ingestion_decision(&classified, &policy);
        // No sensitive fields -> should be accepted
        assert!(decision.is_accepted());
    }

    #[test]
    fn e2e_classify_redact_sensitive_event() {
        let mut classifier = test_classifier();
        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "password": "hunter2",
            "status": "ok"
        });

        let classified = classifier.classify_event(&event).unwrap();
        assert!(classified.has_prohibited());

        let policy = classifier.find_policy("test-connector").unwrap().clone();
        let decision = classifier.ingestion_decision(&classified, &policy);
        assert!(decision.is_rejected());

        // Redact anyway to verify the output
        let redacted = classifier.redact_event(&event, &classified);
        assert!(redacted.event.payload.get("password").is_none());
        assert!(redacted.event.payload.get("status").is_some());
    }

    // ── Disabled rules ──

    #[test]
    fn disabled_rule_not_applied() {
        let mut policy = ClassificationPolicy::default();
        // Disable all rules
        for rule in &mut policy.rules {
            rule.enabled = false;
        }

        let mut classifier = ConnectorDataClassifier::new(ClassifierConfig::default());
        classifier.register_policy(policy);

        let mut event = test_event("test-connector");
        event.payload = serde_json::json!({
            "password": "secret"
        });

        let classified = classifier.classify_event(&event).unwrap();
        // With all rules disabled, password falls to default sensitivity (Internal)
        let pw_fc = classified
            .field_classifications
            .iter()
            .find(|fc| fc.field_path == "payload.password");
        if let Some(fc) = pw_fc {
            assert_eq!(fc.sensitivity, DataSensitivity::Internal);
        }
    }
}
