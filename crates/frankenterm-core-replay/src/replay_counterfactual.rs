//! Counterfactual override package format, loader, and applicator (ft-og6q6.4.1).
//!
//! Provides:
//! - [`OverridePackage`] — Declarative what-if experiment manifest (.ftoverride format).
//! - [`OverridePackageLoader`] — Validates and loads override packages from TOML.
//! - [`OverrideApplicator`] — Matches rule IDs to overrides at decision time.
//! - [`OverrideManifest`] — Hash-pair list for diff detection.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

// ============================================================================
// Override action
// ============================================================================

/// What to do with the matched rule/workflow/policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverrideAction {
    /// Replace the original definition with this override.
    Replace,
    /// Disable the rule entirely (no decision produced).
    Disable,
    /// Add a new rule that didn't exist in baseline.
    Add,
}

impl std::fmt::Display for OverrideAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Replace => write!(f, "replace"),
            Self::Disable => write!(f, "disable"),
            Self::Add => write!(f, "add"),
        }
    }
}

// ============================================================================
// Override entries
// ============================================================================

/// A single pattern-rule override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatternOverride {
    /// Rule ID or wildcard pattern (e.g. "rate_limit_*").
    pub rule_id: String,
    /// What to do with matching rules.
    pub action: OverrideAction,
    /// New definition (inline TOML string or file path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_definition: Option<String>,
    /// Computed hash of the new definition (FNV-1a hex).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_hash: Option<String>,
}

/// A single workflow override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowOverride {
    /// Workflow ID or wildcard pattern.
    pub workflow_id: String,
    /// What to do with matching workflows.
    pub action: OverrideAction,
    /// New workflow steps (inline TOML or file path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_steps: Option<String>,
    /// Computed hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_hash: Option<String>,
}

/// A single policy override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyOverride {
    /// Policy ID or wildcard pattern.
    pub policy_id: String,
    /// What to do with matching policies.
    pub action: OverrideAction,
    /// New policy rules (inline TOML or file path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_rules: Option<String>,
    /// Computed hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definition_hash: Option<String>,
}

// ============================================================================
// OverridePackage — the .ftoverride format
// ============================================================================

/// Metadata section of the override package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideMeta {
    /// Human-readable name of this experiment.
    pub name: String,
    /// Description of what this override tests.
    #[serde(default)]
    pub description: String,
    /// Path to the baseline .ftreplay artifact.
    #[serde(default)]
    pub base_artifact: String,
    /// ISO-8601 timestamp of creation.
    #[serde(default)]
    pub created_at: String,
    /// Author identifier.
    #[serde(default)]
    pub author: String,
}

/// A complete override package (deserializable from TOML).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverridePackage {
    /// Package metadata.
    pub meta: OverrideMeta,
    /// Pattern-rule overrides.
    #[serde(default)]
    pub pattern_overrides: Vec<PatternOverride>,
    /// Workflow overrides.
    #[serde(default)]
    pub workflow_overrides: Vec<WorkflowOverride>,
    /// Policy overrides.
    #[serde(default)]
    pub policy_overrides: Vec<PolicyOverride>,
}

impl OverridePackage {
    /// Total number of overrides.
    #[must_use]
    pub fn override_count(&self) -> usize {
        self.pattern_overrides.len() + self.workflow_overrides.len() + self.policy_overrides.len()
    }

    /// Whether this is an empty override package (baseline replay).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.override_count() == 0
    }

    /// Collect all rule/workflow/policy IDs referenced by overrides.
    #[must_use]
    pub fn all_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for o in &self.pattern_overrides {
            ids.push(o.rule_id.clone());
        }
        for o in &self.workflow_overrides {
            ids.push(o.workflow_id.clone());
        }
        for o in &self.policy_overrides {
            ids.push(o.policy_id.clone());
        }
        ids
    }
}

// ============================================================================
// Definition hashing — FNV-1a
// ============================================================================

/// Compute FNV-1a hash of a definition string, returned as hex.
#[must_use]
pub fn definition_hash(definition: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for byte in definition.as_bytes() {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

// ============================================================================
// Wildcard matching
// ============================================================================

/// Simple wildcard matching: supports `*` at start, end, or both.
///
/// Examples:
/// - `"rate_limit_*"` matches `"rate_limit_foo"`, `"rate_limit_bar"`
/// - `"*_timeout"` matches `"api_timeout"`, `"db_timeout"`
/// - `"*error*"` matches `"my_error_handler"`, `"error"`
/// - `"exact_match"` matches only `"exact_match"`
#[must_use]
pub fn wildcard_matches(pattern: &str, target: &str) -> bool {
    if pattern == target {
        return true;
    }
    if pattern == "*" {
        return true;
    }

    let star_count = pattern.chars().filter(|c| *c == '*').count();
    if star_count == 0 {
        return pattern == target;
    }

    // Split on '*' and match segments in order.
    let parts: Vec<&str> = pattern.split('*').collect();
    let starts_star = pattern.starts_with('*');
    let ends_star = pattern.ends_with('*');

    // Single star cases
    if star_count == 1 {
        if starts_star {
            // "*suffix"
            return target.ends_with(parts[1]);
        }
        if ends_star {
            // "prefix*"
            return target.starts_with(parts[0]);
        }
        // "prefix*suffix"
        let prefix = parts[0];
        let suffix = parts[1];
        return target.len() >= prefix.len() + suffix.len()
            && target.starts_with(prefix)
            && target.ends_with(suffix);
    }

    // General multi-star: greedy left-to-right segment matching.
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 && !starts_star {
            // First segment must be a prefix.
            if !target[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == parts.len() - 1 && !ends_star {
            // Last segment must be a suffix.
            if !target[pos..].ends_with(part) {
                return false;
            }
            pos = target.len();
        } else {
            // Middle segment: find anywhere in remainder.
            if let Some(idx) = target[pos..].find(part) {
                pos += idx + part.len();
            } else {
                return false;
            }
        }
    }
    true
}

// ============================================================================
// Validation errors
// ============================================================================

/// Error from loading or validating an override package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverrideError {
    /// TOML parse error.
    ParseError(String),
    /// Conflicting overrides for the same rule ID.
    ConflictingOverrides {
        rule_id: String,
        actions: Vec<String>,
    },
    /// Referenced rule ID does not exist in baseline artifact.
    UnknownRuleId(String),
    /// Missing required metadata field.
    MissingMeta(String),
    /// Definition hash mismatch.
    HashMismatch {
        rule_id: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for OverrideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError(e) => write!(f, "TOML parse error: {e}"),
            Self::ConflictingOverrides { rule_id, actions } => {
                write!(f, "conflicting overrides for '{rule_id}': {actions:?}")
            }
            Self::UnknownRuleId(id) => write!(f, "unknown rule ID: {id}"),
            Self::MissingMeta(field) => write!(f, "missing metadata field: {field}"),
            Self::HashMismatch {
                rule_id,
                expected,
                actual,
            } => write!(
                f,
                "hash mismatch for '{rule_id}': expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for OverrideError {}

// ============================================================================
// OverridePackageLoader
// ============================================================================

/// Loads and validates .ftoverride packages from TOML strings.
pub struct OverridePackageLoader;

impl OverridePackageLoader {
    /// Parse and validate an override package from TOML.
    ///
    /// Checks for:
    /// - Valid TOML syntax
    /// - No conflicting overrides (same ID with different actions)
    /// - Computes definition hashes for entries with definitions
    pub fn load(toml_str: &str) -> Result<OverridePackage, OverrideError> {
        let mut pkg: OverridePackage =
            toml::from_str(toml_str).map_err(|e| OverrideError::ParseError(e.to_string()))?;

        // Validate: no conflicting overrides (same rule_id with different actions).
        Self::check_pattern_conflicts(&pkg.pattern_overrides)?;
        Self::check_workflow_conflicts(&pkg.workflow_overrides)?;
        Self::check_policy_conflicts(&pkg.policy_overrides)?;

        // Compute definition hashes where definitions are provided.
        for o in &mut pkg.pattern_overrides {
            if let Some(ref def) = o.new_definition {
                let hash = definition_hash(def);
                if let Some(ref declared) = o.definition_hash {
                    if *declared != hash {
                        return Err(OverrideError::HashMismatch {
                            rule_id: o.rule_id.clone(),
                            expected: declared.clone(),
                            actual: hash,
                        });
                    }
                }
                o.definition_hash = Some(hash);
            }
        }
        for o in &mut pkg.workflow_overrides {
            if let Some(ref def) = o.new_steps {
                let hash = definition_hash(def);
                if let Some(ref declared) = o.definition_hash {
                    if *declared != hash {
                        return Err(OverrideError::HashMismatch {
                            rule_id: o.workflow_id.clone(),
                            expected: declared.clone(),
                            actual: hash,
                        });
                    }
                }
                o.definition_hash = Some(hash);
            }
        }
        for o in &mut pkg.policy_overrides {
            if let Some(ref def) = o.new_rules {
                let hash = definition_hash(def);
                if let Some(ref declared) = o.definition_hash {
                    if *declared != hash {
                        return Err(OverrideError::HashMismatch {
                            rule_id: o.policy_id.clone(),
                            expected: declared.clone(),
                            actual: hash,
                        });
                    }
                }
                o.definition_hash = Some(hash);
            }
        }

        Ok(pkg)
    }

    /// Validate rule IDs against a known set of baseline IDs.
    ///
    /// Non-wildcard IDs that don't exist and aren't `Add` actions are rejected.
    pub fn validate_against_baseline(
        pkg: &OverridePackage,
        known_ids: &[String],
    ) -> Result<(), OverrideError> {
        for o in &pkg.pattern_overrides {
            if o.action != OverrideAction::Add && !Self::id_exists(&o.rule_id, known_ids) {
                return Err(OverrideError::UnknownRuleId(o.rule_id.clone()));
            }
        }
        for o in &pkg.workflow_overrides {
            if o.action != OverrideAction::Add && !Self::id_exists(&o.workflow_id, known_ids) {
                return Err(OverrideError::UnknownRuleId(o.workflow_id.clone()));
            }
        }
        for o in &pkg.policy_overrides {
            if o.action != OverrideAction::Add && !Self::id_exists(&o.policy_id, known_ids) {
                return Err(OverrideError::UnknownRuleId(o.policy_id.clone()));
            }
        }
        Ok(())
    }

    fn id_exists(pattern: &str, known_ids: &[String]) -> bool {
        if pattern.contains('*') {
            // Wildcard: at least one known ID must match.
            known_ids.iter().any(|id| wildcard_matches(pattern, id))
        } else {
            known_ids.iter().any(|id| id == pattern)
        }
    }

    fn check_pattern_conflicts(overrides: &[PatternOverride]) -> Result<(), OverrideError> {
        let mut seen: HashMap<String, OverrideAction> = HashMap::new();
        for o in overrides {
            if let Some(prev_action) = seen.get(&o.rule_id) {
                if *prev_action != o.action {
                    return Err(OverrideError::ConflictingOverrides {
                        rule_id: o.rule_id.clone(),
                        actions: vec![prev_action.to_string(), o.action.to_string()],
                    });
                }
            }
            seen.insert(o.rule_id.clone(), o.action);
        }
        Ok(())
    }

    fn check_workflow_conflicts(overrides: &[WorkflowOverride]) -> Result<(), OverrideError> {
        let mut seen: HashMap<String, OverrideAction> = HashMap::new();
        for o in overrides {
            if let Some(prev_action) = seen.get(&o.workflow_id) {
                if *prev_action != o.action {
                    return Err(OverrideError::ConflictingOverrides {
                        rule_id: o.workflow_id.clone(),
                        actions: vec![prev_action.to_string(), o.action.to_string()],
                    });
                }
            }
            seen.insert(o.workflow_id.clone(), o.action);
        }
        Ok(())
    }

    fn check_policy_conflicts(overrides: &[PolicyOverride]) -> Result<(), OverrideError> {
        let mut seen: HashMap<String, OverrideAction> = HashMap::new();
        for o in overrides {
            if let Some(prev_action) = seen.get(&o.policy_id) {
                if *prev_action != o.action {
                    return Err(OverrideError::ConflictingOverrides {
                        rule_id: o.policy_id.clone(),
                        actions: vec![prev_action.to_string(), o.action.to_string()],
                    });
                }
            }
            seen.insert(o.policy_id.clone(), o.action);
        }
        Ok(())
    }
}

// ============================================================================
// OverrideManifest — hash-pair list for diff detection
// ============================================================================

/// A single entry in the override manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Identifier of the overridden item.
    pub item_id: String,
    /// Category: "pattern", "workflow", or "policy".
    pub category: String,
    /// Action taken.
    pub action: OverrideAction,
    /// Hash of the original definition (from baseline).
    pub original_hash: Option<String>,
    /// Hash of the override definition.
    pub override_hash: Option<String>,
}

/// Manifest listing all (original, override) hash pairs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverrideManifest {
    /// Package name.
    pub package_name: String,
    /// Entries.
    pub entries: Vec<ManifestEntry>,
}

impl OverrideManifest {
    /// Build a manifest from a package and optional baseline hash lookup.
    #[must_use]
    pub fn build(pkg: &OverridePackage, baseline_hashes: &BTreeMap<String, String>) -> Self {
        let mut entries = Vec::new();

        for o in &pkg.pattern_overrides {
            let matching_ids = Self::resolve_ids(&o.rule_id, baseline_hashes);
            for mid in matching_ids {
                entries.push(ManifestEntry {
                    item_id: mid.clone(),
                    category: "pattern".to_string(),
                    action: o.action,
                    original_hash: baseline_hashes.get(&mid).cloned(),
                    override_hash: o.definition_hash.clone(),
                });
            }
            // If no matches and it's an Add, create entry for the new ID.
            if o.action == OverrideAction::Add
                && !o.rule_id.contains('*')
                && !baseline_hashes.contains_key(&o.rule_id)
            {
                entries.push(ManifestEntry {
                    item_id: o.rule_id.clone(),
                    category: "pattern".to_string(),
                    action: o.action,
                    original_hash: None,
                    override_hash: o.definition_hash.clone(),
                });
            }
        }

        for o in &pkg.workflow_overrides {
            let matching_ids = Self::resolve_ids(&o.workflow_id, baseline_hashes);
            for mid in matching_ids {
                entries.push(ManifestEntry {
                    item_id: mid.clone(),
                    category: "workflow".to_string(),
                    action: o.action,
                    original_hash: baseline_hashes.get(&mid).cloned(),
                    override_hash: o.definition_hash.clone(),
                });
            }
            if o.action == OverrideAction::Add
                && !o.workflow_id.contains('*')
                && !baseline_hashes.contains_key(&o.workflow_id)
            {
                entries.push(ManifestEntry {
                    item_id: o.workflow_id.clone(),
                    category: "workflow".to_string(),
                    action: o.action,
                    original_hash: None,
                    override_hash: o.definition_hash.clone(),
                });
            }
        }

        for o in &pkg.policy_overrides {
            let matching_ids = Self::resolve_ids(&o.policy_id, baseline_hashes);
            for mid in matching_ids {
                entries.push(ManifestEntry {
                    item_id: mid.clone(),
                    category: "policy".to_string(),
                    action: o.action,
                    original_hash: baseline_hashes.get(&mid).cloned(),
                    override_hash: o.definition_hash.clone(),
                });
            }
            if o.action == OverrideAction::Add
                && !o.policy_id.contains('*')
                && !baseline_hashes.contains_key(&o.policy_id)
            {
                entries.push(ManifestEntry {
                    item_id: o.policy_id.clone(),
                    category: "policy".to_string(),
                    action: o.action,
                    original_hash: None,
                    override_hash: o.definition_hash.clone(),
                });
            }
        }

        Self {
            package_name: pkg.meta.name.clone(),
            entries,
        }
    }

    fn resolve_ids(pattern: &str, baseline: &BTreeMap<String, String>) -> Vec<String> {
        if pattern.contains('*') {
            baseline
                .keys()
                .filter(|k| wildcard_matches(pattern, k))
                .cloned()
                .collect()
        } else if baseline.contains_key(pattern) {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        }
    }
}

// ============================================================================
// Resource-control override packages
// ============================================================================

/// Versioned schema for replay-backed resource-control what-if packages.
pub const RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION: &str = "ft.resource_control_override.v1";

const MAX_RESOURCE_KNOB_ID_LEN: usize = 128;

fn default_resource_override_schema_version() -> String {
    RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION.to_string()
}

fn default_resource_override_action() -> ResourceOverrideAction {
    ResourceOverrideAction::Set
}

/// Resource-control domain targeted by a what-if override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceOverrideDomain {
    /// Global admission/defer/degrade/shed thresholds.
    Admission,
    /// Lane and pane priority weights.
    Qos,
    /// Topology or locality-placement policy.
    Topology,
    /// Hot/warm/cold/search memory tier budgets.
    MemoryTier,
    /// Safe auto-tuning controls.
    AutoTune,
}

impl ResourceOverrideDomain {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Qos => "qos",
            Self::Topology => "topology",
            Self::MemoryTier => "memory_tier",
            Self::AutoTune => "auto_tune",
        }
    }
}

/// Primitive value shape accepted by a safe resource-control knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKnobValueKind {
    Bool,
    Integer,
    Float,
    Text,
}

/// Action to take for a resource-control knob in a what-if run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceOverrideAction {
    /// Set the knob to a candidate value.
    Set,
    /// Disable the knob or policy, when explicitly allowed by the registry.
    Disable,
    /// Reset the knob to its baseline/default value.
    ResetToDefault,
}

impl ResourceOverrideAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Disable => "disable",
            Self::ResetToDefault => "reset_to_default",
        }
    }
}

/// Static safe-knob registry entry for resource-control what-if packages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceKnobSpec {
    pub knob_id: &'static str,
    pub domain: ResourceOverrideDomain,
    pub value_kind: ResourceKnobValueKind,
    pub min_value: Option<f64>,
    pub max_value: Option<f64>,
    pub allow_disable: bool,
    pub description: &'static str,
}

impl ResourceKnobSpec {
    pub fn validates_value(self, raw: &str) -> Result<(), ResourceOverrideError> {
        match self.value_kind {
            ResourceKnobValueKind::Bool => match raw {
                "true" | "false" => Ok(()),
                _ => Err(ResourceOverrideError::ValueTypeMismatch {
                    knob_id: self.knob_id.to_string(),
                    expected: self.value_kind,
                    value: raw.to_string(),
                }),
            },
            ResourceKnobValueKind::Integer => {
                let value =
                    raw.parse::<i64>()
                        .map_err(|_| ResourceOverrideError::ValueTypeMismatch {
                            knob_id: self.knob_id.to_string(),
                            expected: self.value_kind,
                            value: raw.to_string(),
                        })?;
                self.validate_numeric(value as f64, raw)
            }
            ResourceKnobValueKind::Float => {
                let value =
                    raw.parse::<f64>()
                        .map_err(|_| ResourceOverrideError::ValueTypeMismatch {
                            knob_id: self.knob_id.to_string(),
                            expected: self.value_kind,
                            value: raw.to_string(),
                        })?;
                if !value.is_finite() {
                    return Err(ResourceOverrideError::ValueTypeMismatch {
                        knob_id: self.knob_id.to_string(),
                        expected: self.value_kind,
                        value: raw.to_string(),
                    });
                }
                self.validate_numeric(value, raw)
            }
            ResourceKnobValueKind::Text => {
                if raw.trim().is_empty() {
                    return Err(ResourceOverrideError::ValueTypeMismatch {
                        knob_id: self.knob_id.to_string(),
                        expected: self.value_kind,
                        value: raw.to_string(),
                    });
                }
                Ok(())
            }
        }
    }

    fn validate_numeric(self, value: f64, raw: &str) -> Result<(), ResourceOverrideError> {
        let below_min = self.min_value.is_some_and(|min| value < min);
        let above_max = self.max_value.is_some_and(|max| value > max);
        if below_min || above_max {
            return Err(ResourceOverrideError::ValueOutOfRange {
                knob_id: self.knob_id.to_string(),
                min: self.min_value,
                max: self.max_value,
                value: raw.to_string(),
            });
        }
        Ok(())
    }
}

/// Metadata for a resource-control what-if package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceControlOverrideMeta {
    /// Schema version. Defaults to [`RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION`].
    #[serde(default = "default_resource_override_schema_version")]
    pub schema_version: String,
    /// Human-readable experiment name.
    pub name: String,
    /// Description of what the candidate resource policy tests.
    #[serde(default)]
    pub description: String,
    /// Path or label of the baseline trace artifact.
    #[serde(default)]
    pub base_trace: String,
    /// ISO-8601 creation timestamp.
    #[serde(default)]
    pub created_at: String,
    /// Author or agent identifier.
    #[serde(default)]
    pub author: String,
}

/// One safe resource-control knob override for a what-if run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceControlOverride {
    /// Stable knob identifier from the safe-knob registry.
    pub knob_id: String,
    /// Expected domain for the knob. Must match the registry entry.
    pub domain: ResourceOverrideDomain,
    /// What to do with this knob.
    #[serde(default = "default_resource_override_action")]
    pub action: ResourceOverrideAction,
    /// Candidate value, encoded as a stable string to avoid float/TOML drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Operator-facing reason for this override.
    #[serde(default)]
    pub reason: String,
    /// Optional declared hash. The loader computes and verifies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_hash: Option<String>,
}

/// Complete resource-control what-if package.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceControlOverridePackage {
    pub meta: ResourceControlOverrideMeta,
    /// Backward-compatible flat override list for compact packages.
    #[serde(default)]
    pub overrides: Vec<ResourceControlOverride>,
    /// Admission threshold and pressure-control overrides.
    #[serde(default)]
    pub admission: Vec<ResourceControlOverride>,
    /// QoS class and scheduling-weight overrides.
    #[serde(default)]
    pub qos: Vec<ResourceControlOverride>,
    /// Topology placement and rebalance overrides.
    #[serde(default)]
    pub topology: Vec<ResourceControlOverride>,
    /// Hot/warm/cold/search memory-tier budget overrides.
    #[serde(default)]
    pub memory_tier: Vec<ResourceControlOverride>,
    /// Replay-only auto-tune candidate knob overrides.
    #[serde(default)]
    pub auto_tune: Vec<ResourceControlOverride>,
}

impl ResourceControlOverridePackage {
    #[must_use]
    pub fn override_count(&self) -> usize {
        self.all_overrides().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.override_count() == 0
    }

    #[must_use]
    pub fn knob_ids(&self) -> Vec<String> {
        let mut ids = self
            .all_overrides()
            .into_iter()
            .map(|override_| override_.knob_id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    #[must_use]
    pub fn all_overrides(&self) -> Vec<&ResourceControlOverride> {
        self.overrides
            .iter()
            .chain(self.admission.iter())
            .chain(self.qos.iter())
            .chain(self.topology.iter())
            .chain(self.memory_tier.iter())
            .chain(self.auto_tune.iter())
            .collect()
    }
}

/// Error from loading or validating a resource-control what-if package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResourceOverrideError {
    ParseError(String),
    UnsupportedSchemaVersion {
        expected: String,
        actual: String,
    },
    MissingMeta(String),
    UnknownKnob(String),
    UnsafeWildcard(String),
    DomainMismatch {
        knob_id: String,
        expected: ResourceOverrideDomain,
        actual: ResourceOverrideDomain,
    },
    ConflictingKnobOverrides(String),
    MissingValue(String),
    DisableNotAllowed(String),
    ValueTypeMismatch {
        knob_id: String,
        expected: ResourceKnobValueKind,
        value: String,
    },
    ValueOutOfRange {
        knob_id: String,
        min: Option<f64>,
        max: Option<f64>,
        value: String,
    },
    HashMismatch {
        knob_id: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for ResourceOverrideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ParseError(error) => write!(f, "TOML parse error: {error}"),
            Self::UnsupportedSchemaVersion { expected, actual } => {
                write!(
                    f,
                    "unsupported resource override schema {actual}; expected {expected}"
                )
            }
            Self::MissingMeta(field) => write!(f, "missing resource override metadata: {field}"),
            Self::UnknownKnob(knob_id) => write!(f, "unknown resource-control knob: {knob_id}"),
            Self::UnsafeWildcard(knob_id) => {
                write!(f, "resource-control knob wildcards are unsafe: {knob_id}")
            }
            Self::DomainMismatch {
                knob_id,
                expected,
                actual,
            } => write!(
                f,
                "domain mismatch for {knob_id}: expected {}, got {}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::ConflictingKnobOverrides(knob_id) => {
                write!(f, "conflicting resource-control override for {knob_id}")
            }
            Self::MissingValue(knob_id) => write!(f, "missing value for resource knob {knob_id}"),
            Self::DisableNotAllowed(knob_id) => {
                write!(f, "disable is not allowed for resource knob {knob_id}")
            }
            Self::ValueTypeMismatch {
                knob_id,
                expected,
                value,
            } => write!(
                f,
                "value {value:?} does not match {expected:?} for resource knob {knob_id}"
            ),
            Self::ValueOutOfRange {
                knob_id,
                min,
                max,
                value,
            } => write!(
                f,
                "value {value:?} is outside range {min:?}..={max:?} for resource knob {knob_id}"
            ),
            Self::HashMismatch {
                knob_id,
                expected,
                actual,
            } => write!(
                f,
                "hash mismatch for resource knob {knob_id}: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ResourceOverrideError {}

/// Loader and validator for resource-control what-if packages.
pub struct ResourceControlOverrideLoader;

impl ResourceControlOverrideLoader {
    /// Parse and validate a resource-control what-if package.
    pub fn load(toml_str: &str) -> Result<ResourceControlOverridePackage, ResourceOverrideError> {
        let mut pkg: ResourceControlOverridePackage = toml::from_str(toml_str)
            .map_err(|error| ResourceOverrideError::ParseError(error.to_string()))?;
        Self::validate_meta(&pkg.meta)?;
        Self::validate_package_overrides(&mut pkg, &Self::safe_knob_map())?;
        Ok(pkg)
    }

    /// Built-in safe-knob registry for replay-only resource-control experiments.
    #[must_use]
    pub fn builtin_safe_knobs() -> Vec<ResourceKnobSpec> {
        vec![
            ResourceKnobSpec {
                knob_id: "admission.max_queue_utilization",
                domain: ResourceOverrideDomain::Admission,
                value_kind: ResourceKnobValueKind::Float,
                min_value: Some(0.0),
                max_value: Some(1.0),
                allow_disable: false,
                description: "maximum ready-queue utilization before degradation",
            },
            ResourceKnobSpec {
                knob_id: "admission.max_pending_items",
                domain: ResourceOverrideDomain::Admission,
                value_kind: ResourceKnobValueKind::Integer,
                min_value: Some(1.0),
                max_value: Some(1_000_000.0),
                allow_disable: false,
                description: "maximum pending work items before shedding is considered",
            },
            ResourceKnobSpec {
                knob_id: "qos.interactive_weight",
                domain: ResourceOverrideDomain::Qos,
                value_kind: ResourceKnobValueKind::Float,
                min_value: Some(0.1),
                max_value: Some(10.0),
                allow_disable: false,
                description: "relative weight for interactive pane work",
            },
            ResourceKnobSpec {
                knob_id: "qos.bulk_search_weight",
                domain: ResourceOverrideDomain::Qos,
                value_kind: ResourceKnobValueKind::Float,
                min_value: Some(0.1),
                max_value: Some(10.0),
                allow_disable: false,
                description: "relative weight for bulk search/index work",
            },
            ResourceKnobSpec {
                knob_id: "topology.max_migrations_per_epoch",
                domain: ResourceOverrideDomain::Topology,
                value_kind: ResourceKnobValueKind::Integer,
                min_value: Some(0.0),
                max_value: Some(10_000.0),
                allow_disable: false,
                description: "maximum topology rebalance moves per simulation epoch",
            },
            ResourceKnobSpec {
                knob_id: "topology.locality_spread_factor",
                domain: ResourceOverrideDomain::Topology,
                value_kind: ResourceKnobValueKind::Float,
                min_value: Some(0.0),
                max_value: Some(1.0),
                allow_disable: false,
                description: "fractional preference for spreading work across locality groups",
            },
            ResourceKnobSpec {
                knob_id: "memory.hot_resident_budget_bytes",
                domain: ResourceOverrideDomain::MemoryTier,
                value_kind: ResourceKnobValueKind::Integer,
                min_value: Some(1_048_576.0),
                max_value: Some(1_099_511_627_776.0),
                allow_disable: false,
                description: "hot resident memory budget in bytes",
            },
            ResourceKnobSpec {
                knob_id: "memory.search_cache_budget_bytes",
                domain: ResourceOverrideDomain::MemoryTier,
                value_kind: ResourceKnobValueKind::Integer,
                min_value: Some(1_048_576.0),
                max_value: Some(1_099_511_627_776.0),
                allow_disable: false,
                description: "search/index cache memory budget in bytes",
            },
            ResourceKnobSpec {
                knob_id: "autotune.enabled",
                domain: ResourceOverrideDomain::AutoTune,
                value_kind: ResourceKnobValueKind::Bool,
                min_value: None,
                max_value: None,
                allow_disable: true,
                description: "whether replay-only auto-tune candidates are considered",
            },
            ResourceKnobSpec {
                knob_id: "autotune.exploration_budget_percent",
                domain: ResourceOverrideDomain::AutoTune,
                value_kind: ResourceKnobValueKind::Float,
                min_value: Some(0.0),
                max_value: Some(20.0),
                allow_disable: false,
                description: "maximum percent of budget spent on candidate exploration",
            },
        ]
    }

    fn safe_knob_map() -> BTreeMap<&'static str, ResourceKnobSpec> {
        Self::builtin_safe_knobs()
            .into_iter()
            .map(|spec| (spec.knob_id, spec))
            .collect()
    }

    fn validate_meta(meta: &ResourceControlOverrideMeta) -> Result<(), ResourceOverrideError> {
        if meta.schema_version != RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION {
            return Err(ResourceOverrideError::UnsupportedSchemaVersion {
                expected: RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION.to_string(),
                actual: meta.schema_version.clone(),
            });
        }
        if meta.name.trim().is_empty() {
            return Err(ResourceOverrideError::MissingMeta("name".to_string()));
        }
        Ok(())
    }

    fn validate_package_overrides(
        pkg: &mut ResourceControlOverridePackage,
        registry: &BTreeMap<&'static str, ResourceKnobSpec>,
    ) -> Result<(), ResourceOverrideError> {
        let mut seen = BTreeSet::<String>::new();
        Self::validate_override_group(&mut pkg.overrides, None, registry, &mut seen)?;
        Self::validate_override_group(
            &mut pkg.admission,
            Some(ResourceOverrideDomain::Admission),
            registry,
            &mut seen,
        )?;
        Self::validate_override_group(
            &mut pkg.qos,
            Some(ResourceOverrideDomain::Qos),
            registry,
            &mut seen,
        )?;
        Self::validate_override_group(
            &mut pkg.topology,
            Some(ResourceOverrideDomain::Topology),
            registry,
            &mut seen,
        )?;
        Self::validate_override_group(
            &mut pkg.memory_tier,
            Some(ResourceOverrideDomain::MemoryTier),
            registry,
            &mut seen,
        )?;
        Self::validate_override_group(
            &mut pkg.auto_tune,
            Some(ResourceOverrideDomain::AutoTune),
            registry,
            &mut seen,
        )
    }

    fn validate_override_group(
        overrides: &mut [ResourceControlOverride],
        section_domain: Option<ResourceOverrideDomain>,
        registry: &BTreeMap<&'static str, ResourceKnobSpec>,
        seen: &mut BTreeSet<String>,
    ) -> Result<(), ResourceOverrideError> {
        for override_ in &mut *overrides {
            Self::validate_knob_id(&override_.knob_id)?;
            if !seen.insert(override_.knob_id.clone()) {
                return Err(ResourceOverrideError::ConflictingKnobOverrides(
                    override_.knob_id.clone(),
                ));
            }
            if let Some(expected) = section_domain {
                if override_.domain != expected {
                    return Err(ResourceOverrideError::DomainMismatch {
                        knob_id: override_.knob_id.clone(),
                        expected,
                        actual: override_.domain,
                    });
                }
            }
            let spec = registry
                .get(override_.knob_id.as_str())
                .copied()
                .ok_or_else(|| ResourceOverrideError::UnknownKnob(override_.knob_id.clone()))?;
            if override_.domain != spec.domain {
                return Err(ResourceOverrideError::DomainMismatch {
                    knob_id: override_.knob_id.clone(),
                    expected: spec.domain,
                    actual: override_.domain,
                });
            }
            Self::validate_override_value(override_, spec)?;
            let hash = resource_override_hash(override_);
            if let Some(declared) = &override_.value_hash {
                if *declared != hash {
                    return Err(ResourceOverrideError::HashMismatch {
                        knob_id: override_.knob_id.clone(),
                        expected: declared.clone(),
                        actual: hash,
                    });
                }
            }
            override_.value_hash = Some(hash);
        }
        overrides.sort_by(|left, right| left.knob_id.cmp(&right.knob_id));
        Ok(())
    }

    fn validate_knob_id(knob_id: &str) -> Result<(), ResourceOverrideError> {
        if knob_id.trim().is_empty() || knob_id.len() > MAX_RESOURCE_KNOB_ID_LEN {
            return Err(ResourceOverrideError::UnknownKnob(knob_id.to_string()));
        }
        if knob_id.contains('*') {
            return Err(ResourceOverrideError::UnsafeWildcard(knob_id.to_string()));
        }
        Ok(())
    }

    fn validate_override_value(
        override_: &ResourceControlOverride,
        spec: ResourceKnobSpec,
    ) -> Result<(), ResourceOverrideError> {
        match override_.action {
            ResourceOverrideAction::Set => {
                let value = override_.value.as_deref().ok_or_else(|| {
                    ResourceOverrideError::MissingValue(override_.knob_id.clone())
                })?;
                spec.validates_value(value)
            }
            ResourceOverrideAction::Disable => {
                if !spec.allow_disable {
                    return Err(ResourceOverrideError::DisableNotAllowed(
                        override_.knob_id.clone(),
                    ));
                }
                Ok(())
            }
            ResourceOverrideAction::ResetToDefault => Ok(()),
        }
    }
}

/// Stable hash for a resource-control knob override.
#[must_use]
pub fn resource_override_hash(override_: &ResourceControlOverride) -> String {
    let value = override_.value.as_deref().unwrap_or("");
    definition_hash(&format!(
        "{}|{}|{}|{}",
        override_.domain.as_str(),
        override_.knob_id,
        override_.action.as_str(),
        value
    ))
}

/// Manifest entry for a resource-control what-if package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceManifestEntry {
    pub knob_id: String,
    pub domain: ResourceOverrideDomain,
    pub action: ResourceOverrideAction,
    pub baseline_hash: Option<String>,
    pub override_hash: Option<String>,
    pub reason: String,
}

/// Deterministic manifest for resource-control what-if package diffs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceControlOverrideManifest {
    pub schema_version: String,
    pub package_name: String,
    pub entries: Vec<ResourceManifestEntry>,
}

impl ResourceControlOverrideManifest {
    #[must_use]
    pub fn build(
        pkg: &ResourceControlOverridePackage,
        baseline_hashes: &BTreeMap<String, String>,
    ) -> Self {
        let mut entries = pkg
            .all_overrides()
            .into_iter()
            .map(|override_| ResourceManifestEntry {
                knob_id: override_.knob_id.clone(),
                domain: override_.domain,
                action: override_.action,
                baseline_hash: baseline_hashes.get(&override_.knob_id).cloned(),
                override_hash: override_.value_hash.clone(),
                reason: override_.reason.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.knob_id.cmp(&right.knob_id));
        Self {
            schema_version: RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION.to_string(),
            package_name: pkg.meta.name.clone(),
            entries,
        }
    }
}

// ============================================================================
// OverrideApplicator — runtime lookup
// ============================================================================

/// A substitution record emitted when an override is applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubstitutionRecord {
    /// Which rule/workflow/policy was overridden.
    pub item_id: String,
    /// Category.
    pub category: String,
    /// Action taken.
    pub action: OverrideAction,
    /// Hash of the original definition.
    pub original_hash: Option<String>,
    /// Hash of the override definition.
    pub override_hash: Option<String>,
}

/// Applies overrides at decision time and records substitutions.
pub struct OverrideApplicator {
    /// Pattern overrides indexed by exact rule_id; wildcards stored separately.
    exact_patterns: HashMap<String, PatternOverride>,
    wildcard_patterns: Vec<PatternOverride>,
    /// Workflow overrides.
    exact_workflows: HashMap<String, WorkflowOverride>,
    wildcard_workflows: Vec<WorkflowOverride>,
    /// Policy overrides.
    exact_policies: HashMap<String, PolicyOverride>,
    wildcard_policies: Vec<PolicyOverride>,
    /// Substitution log.
    inner: Mutex<ApplicatorInner>,
}

struct ApplicatorInner {
    substitutions: Vec<SubstitutionRecord>,
}

/// The result of looking up an override for a rule ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookupResult {
    /// No override; use baseline definition.
    NoOverride,
    /// Rule is disabled; produce no decision.
    Disabled,
    /// Replace with this definition.
    Replace(String),
    /// Add this new definition.
    Add(String),
}

impl OverrideApplicator {
    /// Create from an override package.
    #[must_use]
    pub fn new(pkg: &OverridePackage) -> Self {
        let mut exact_patterns = HashMap::new();
        let mut wildcard_patterns = Vec::new();
        for o in &pkg.pattern_overrides {
            if o.rule_id.contains('*') {
                wildcard_patterns.push(o.clone());
            } else {
                exact_patterns.insert(o.rule_id.clone(), o.clone());
            }
        }
        let mut exact_workflows = HashMap::new();
        let mut wildcard_workflows = Vec::new();
        for o in &pkg.workflow_overrides {
            if o.workflow_id.contains('*') {
                wildcard_workflows.push(o.clone());
            } else {
                exact_workflows.insert(o.workflow_id.clone(), o.clone());
            }
        }
        let mut exact_policies = HashMap::new();
        let mut wildcard_policies = Vec::new();
        for o in &pkg.policy_overrides {
            if o.policy_id.contains('*') {
                wildcard_policies.push(o.clone());
            } else {
                exact_policies.insert(o.policy_id.clone(), o.clone());
            }
        }
        Self {
            exact_patterns,
            wildcard_patterns,
            exact_workflows,
            wildcard_workflows,
            exact_policies,
            wildcard_policies,
            inner: Mutex::new(ApplicatorInner {
                substitutions: Vec::new(),
            }),
        }
    }

    /// Look up override for a pattern rule.
    pub fn lookup_pattern(&self, rule_id: &str, original_hash: Option<&str>) -> LookupResult {
        if let Some(o) = self.exact_patterns.get(rule_id) {
            return self.apply_pattern_override(o, rule_id, original_hash);
        }
        for o in &self.wildcard_patterns {
            if wildcard_matches(&o.rule_id, rule_id) {
                return self.apply_pattern_override(o, rule_id, original_hash);
            }
        }
        LookupResult::NoOverride
    }

    /// Look up override for a workflow.
    pub fn lookup_workflow(&self, workflow_id: &str, original_hash: Option<&str>) -> LookupResult {
        if let Some(o) = self.exact_workflows.get(workflow_id) {
            return self.apply_workflow_override(o, workflow_id, original_hash);
        }
        for o in &self.wildcard_workflows {
            if wildcard_matches(&o.workflow_id, workflow_id) {
                return self.apply_workflow_override(o, workflow_id, original_hash);
            }
        }
        LookupResult::NoOverride
    }

    /// Look up override for a policy.
    pub fn lookup_policy(&self, policy_id: &str, original_hash: Option<&str>) -> LookupResult {
        if let Some(o) = self.exact_policies.get(policy_id) {
            return self.apply_policy_override(o, policy_id, original_hash);
        }
        for o in &self.wildcard_policies {
            if wildcard_matches(&o.policy_id, policy_id) {
                return self.apply_policy_override(o, policy_id, original_hash);
            }
        }
        LookupResult::NoOverride
    }

    /// Get all substitution records.
    #[must_use]
    pub fn substitutions(&self) -> Vec<SubstitutionRecord> {
        self.inner.lock().unwrap().substitutions.clone()
    }

    /// Number of substitutions applied.
    #[must_use]
    pub fn substitution_count(&self) -> usize {
        self.inner.lock().unwrap().substitutions.len()
    }

    fn record_substitution(
        &self,
        item_id: &str,
        category: &str,
        action: OverrideAction,
        original_hash: Option<&str>,
        override_hash: Option<&str>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        inner.substitutions.push(SubstitutionRecord {
            item_id: item_id.to_string(),
            category: category.to_string(),
            action,
            original_hash: original_hash.map(String::from),
            override_hash: override_hash.map(String::from),
        });
    }

    fn apply_pattern_override(
        &self,
        o: &PatternOverride,
        rule_id: &str,
        original_hash: Option<&str>,
    ) -> LookupResult {
        self.record_substitution(
            rule_id,
            "pattern",
            o.action,
            original_hash,
            o.definition_hash.as_deref(),
        );
        match o.action {
            OverrideAction::Disable => LookupResult::Disabled,
            OverrideAction::Replace => {
                LookupResult::Replace(o.new_definition.clone().unwrap_or_default())
            }
            OverrideAction::Add => LookupResult::Add(o.new_definition.clone().unwrap_or_default()),
        }
    }

    fn apply_workflow_override(
        &self,
        o: &WorkflowOverride,
        workflow_id: &str,
        original_hash: Option<&str>,
    ) -> LookupResult {
        self.record_substitution(
            workflow_id,
            "workflow",
            o.action,
            original_hash,
            o.definition_hash.as_deref(),
        );
        match o.action {
            OverrideAction::Disable => LookupResult::Disabled,
            OverrideAction::Replace => {
                LookupResult::Replace(o.new_steps.clone().unwrap_or_default())
            }
            OverrideAction::Add => LookupResult::Add(o.new_steps.clone().unwrap_or_default()),
        }
    }

    fn apply_policy_override(
        &self,
        o: &PolicyOverride,
        policy_id: &str,
        original_hash: Option<&str>,
    ) -> LookupResult {
        self.record_substitution(
            policy_id,
            "policy",
            o.action,
            original_hash,
            o.definition_hash.as_deref(),
        );
        match o.action {
            OverrideAction::Disable => LookupResult::Disabled,
            OverrideAction::Replace => {
                LookupResult::Replace(o.new_rules.clone().unwrap_or_default())
            }
            OverrideAction::Add => LookupResult::Add(o.new_rules.clone().unwrap_or_default()),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn sample_toml() -> &'static str {
        r#"
[meta]
name = "test-experiment"
description = "Test override"
base_artifact = "baseline.ftreplay"
created_at = "2026-01-01T00:00:00Z"
author = "tester"

[[pattern_overrides]]
rule_id = "rate_limit_api"
action = "replace"
new_definition = "threshold = 100"

[[pattern_overrides]]
rule_id = "error_detect_timeout"
action = "disable"

[[workflow_overrides]]
workflow_id = "deploy_canary"
action = "replace"
new_steps = "step1 = 'validate'\nstep2 = 'deploy'"

[[policy_overrides]]
policy_id = "max_retries"
action = "replace"
new_rules = "retries = 5"
"#
    }

    fn minimal_toml() -> &'static str {
        r#"
[meta]
name = "empty-experiment"
"#
    }

    fn wildcard_toml() -> &'static str {
        r#"
[meta]
name = "wildcard-test"

[[pattern_overrides]]
rule_id = "rate_limit_*"
action = "disable"
"#
    }

    // ── Parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parse_full_package() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        assert_eq!(pkg.meta.name, "test-experiment");
        assert_eq!(pkg.pattern_overrides.len(), 2);
        assert_eq!(pkg.workflow_overrides.len(), 1);
        assert_eq!(pkg.policy_overrides.len(), 1);
        assert_eq!(pkg.override_count(), 4);
    }

    #[test]
    fn parse_minimal_package() {
        let pkg = OverridePackageLoader::load(minimal_toml()).unwrap();
        assert!(pkg.is_empty());
        assert_eq!(pkg.override_count(), 0);
    }

    #[test]
    fn parse_only_patterns() {
        let toml = r#"
[meta]
name = "patterns-only"

[[pattern_overrides]]
rule_id = "foo"
action = "replace"
new_definition = "bar"
"#;
        let pkg = OverridePackageLoader::load(toml).unwrap();
        assert_eq!(pkg.pattern_overrides.len(), 1);
        assert!(pkg.workflow_overrides.is_empty());
        assert!(pkg.policy_overrides.is_empty());
    }

    #[test]
    fn parse_invalid_toml() {
        let result = OverridePackageLoader::load("not valid toml {{{");
        assert!(result.is_err());
        let err = result.unwrap_err();
        let is_parse = matches!(err, OverrideError::ParseError(_));
        assert!(is_parse);
    }

    // ── Conflict detection ──────────────────────────────────────────────

    #[test]
    fn conflicting_pattern_overrides() {
        let toml = r#"
[meta]
name = "conflict"

[[pattern_overrides]]
rule_id = "foo"
action = "replace"
new_definition = "bar"

[[pattern_overrides]]
rule_id = "foo"
action = "disable"
"#;
        let result = OverridePackageLoader::load(toml);
        assert!(result.is_err());
        let is_conflict = matches!(
            result.unwrap_err(),
            OverrideError::ConflictingOverrides { .. }
        );
        assert!(is_conflict);
    }

    #[test]
    fn conflicting_workflow_overrides() {
        let toml = r#"
[meta]
name = "conflict"

[[workflow_overrides]]
workflow_id = "wf1"
action = "replace"
new_steps = "step1"

[[workflow_overrides]]
workflow_id = "wf1"
action = "disable"
"#;
        let result = OverridePackageLoader::load(toml);
        assert!(result.is_err());
    }

    #[test]
    fn same_action_not_conflict() {
        let toml = r#"
[meta]
name = "no-conflict"

[[pattern_overrides]]
rule_id = "foo"
action = "replace"
new_definition = "v1"

[[pattern_overrides]]
rule_id = "foo"
action = "replace"
new_definition = "v2"
"#;
        let result = OverridePackageLoader::load(toml);
        assert!(result.is_ok());
    }

    // ── Baseline validation ─────────────────────────────────────────────

    #[test]
    fn validate_known_ids() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let known = vec![
            "rate_limit_api".to_string(),
            "error_detect_timeout".to_string(),
            "deploy_canary".to_string(),
            "max_retries".to_string(),
        ];
        let result = OverridePackageLoader::validate_against_baseline(&pkg, &known);
        assert!(result.is_ok());
    }

    #[test]
    fn reject_unknown_rule_id() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let known = vec!["rate_limit_api".to_string()]; // Missing others.
        let result = OverridePackageLoader::validate_against_baseline(&pkg, &known);
        assert!(result.is_err());
        let is_unknown = matches!(result.unwrap_err(), OverrideError::UnknownRuleId(_));
        assert!(is_unknown);
    }

    #[test]
    fn add_action_skips_unknown_check() {
        let toml = r#"
[meta]
name = "add-test"

[[pattern_overrides]]
rule_id = "new_rule"
action = "add"
new_definition = "threshold = 50"
"#;
        let pkg = OverridePackageLoader::load(toml).unwrap();
        let result = OverridePackageLoader::validate_against_baseline(&pkg, &[]);
        assert!(result.is_ok());
    }

    // ── Wildcard matching ───────────────────────────────────────────────

    #[test]
    fn wildcard_suffix() {
        assert!(wildcard_matches("rate_limit_*", "rate_limit_api"));
        assert!(wildcard_matches("rate_limit_*", "rate_limit_db"));
        assert!(!wildcard_matches("rate_limit_*", "error_detect"));
    }

    #[test]
    fn wildcard_prefix() {
        assert!(wildcard_matches("*_timeout", "api_timeout"));
        assert!(wildcard_matches("*_timeout", "db_timeout"));
        assert!(!wildcard_matches("*_timeout", "timeout_api"));
    }

    #[test]
    fn wildcard_contains() {
        assert!(wildcard_matches("*error*", "my_error_handler"));
        assert!(wildcard_matches("*error*", "error"));
        assert!(!wildcard_matches("*error*", "errthing"));
    }

    #[test]
    fn wildcard_star_only() {
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("*", ""));
    }

    #[test]
    fn wildcard_exact() {
        assert!(wildcard_matches("exact", "exact"));
        assert!(!wildcard_matches("exact", "not_exact"));
    }

    #[test]
    fn wildcard_validates_against_baseline() {
        let pkg = OverridePackageLoader::load(wildcard_toml()).unwrap();
        let known = vec![
            "rate_limit_api".to_string(),
            "rate_limit_db".to_string(),
            "error_detect".to_string(),
        ];
        let result = OverridePackageLoader::validate_against_baseline(&pkg, &known);
        assert!(result.is_ok());
    }

    #[test]
    fn wildcard_no_match_rejects() {
        let toml = r#"
[meta]
name = "no-match"

[[pattern_overrides]]
rule_id = "nonexistent_*"
action = "disable"
"#;
        let pkg = OverridePackageLoader::load(toml).unwrap();
        let known = vec!["rate_limit_api".to_string()];
        let result = OverridePackageLoader::validate_against_baseline(&pkg, &known);
        assert!(result.is_err());
    }

    // ── Definition hashing ──────────────────────────────────────────────

    #[test]
    fn hash_deterministic() {
        let h1 = definition_hash("threshold = 100");
        let h2 = definition_hash("threshold = 100");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16); // 64-bit FNV-1a = 16 hex chars.
    }

    #[test]
    fn hash_differs_for_different_input() {
        let h1 = definition_hash("threshold = 100");
        let h2 = definition_hash("threshold = 200");
        assert_ne!(h1, h2);
    }

    #[test]
    fn loader_computes_hash() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let first = &pkg.pattern_overrides[0];
        assert!(first.definition_hash.is_some());
        let expected = definition_hash("threshold = 100");
        assert_eq!(first.definition_hash.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn hash_mismatch_rejected() {
        let toml = r#"
[meta]
name = "mismatch"

[[pattern_overrides]]
rule_id = "foo"
action = "replace"
new_definition = "bar"
definition_hash = "0000000000000000"
"#;
        let result = OverridePackageLoader::load(toml);
        assert!(result.is_err());
        let is_mismatch = matches!(result.unwrap_err(), OverrideError::HashMismatch { .. });
        assert!(is_mismatch);
    }

    // ── OverrideManifest ────────────────────────────────────────────────

    #[test]
    fn manifest_build() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let mut baseline = BTreeMap::new();
        baseline.insert("rate_limit_api".to_string(), "orig_hash_1".to_string());
        baseline.insert(
            "error_detect_timeout".to_string(),
            "orig_hash_2".to_string(),
        );
        baseline.insert("deploy_canary".to_string(), "orig_hash_3".to_string());
        baseline.insert("max_retries".to_string(), "orig_hash_4".to_string());

        let manifest = OverrideManifest::build(&pkg, &baseline);
        assert_eq!(manifest.package_name, "test-experiment");
        assert_eq!(manifest.entries.len(), 4);
    }

    #[test]
    fn manifest_wildcard_expands() {
        let pkg = OverridePackageLoader::load(wildcard_toml()).unwrap();
        let mut baseline = BTreeMap::new();
        baseline.insert("rate_limit_api".to_string(), "h1".to_string());
        baseline.insert("rate_limit_db".to_string(), "h2".to_string());
        baseline.insert("error_detect".to_string(), "h3".to_string());

        let manifest = OverrideManifest::build(&pkg, &baseline);
        // Should expand rate_limit_* to two entries.
        assert_eq!(manifest.entries.len(), 2);
        assert!(
            manifest
                .entries
                .iter()
                .all(|e| e.action == OverrideAction::Disable)
        );
    }

    #[test]
    fn manifest_add_creates_entry() {
        let toml = r#"
[meta]
name = "add"

[[pattern_overrides]]
rule_id = "new_rule"
action = "add"
new_definition = "def"
"#;
        let pkg = OverridePackageLoader::load(toml).unwrap();
        let manifest = OverrideManifest::build(&pkg, &BTreeMap::new());
        assert_eq!(manifest.entries.len(), 1);
        assert!(manifest.entries[0].original_hash.is_none());
        assert!(manifest.entries[0].override_hash.is_some());
    }

    // ── OverrideApplicator ──────────────────────────────────────────────

    #[test]
    fn applicator_exact_replace() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_pattern("rate_limit_api", Some("orig"));
        assert_eq!(result, LookupResult::Replace("threshold = 100".to_string()));
        assert_eq!(app.substitution_count(), 1);
    }

    #[test]
    fn applicator_exact_disable() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_pattern("error_detect_timeout", None);
        assert_eq!(result, LookupResult::Disabled);
    }

    #[test]
    fn applicator_no_override() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_pattern("nonexistent_rule", None);
        assert_eq!(result, LookupResult::NoOverride);
        assert_eq!(app.substitution_count(), 0);
    }

    #[test]
    fn applicator_wildcard_match() {
        let pkg = OverridePackageLoader::load(wildcard_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let r1 = app.lookup_pattern("rate_limit_api", None);
        assert_eq!(r1, LookupResult::Disabled);
        let r2 = app.lookup_pattern("rate_limit_db", None);
        assert_eq!(r2, LookupResult::Disabled);
        let r3 = app.lookup_pattern("error_detect", None);
        assert_eq!(r3, LookupResult::NoOverride);
        assert_eq!(app.substitution_count(), 2);
    }

    #[test]
    fn applicator_workflow_lookup() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_workflow("deploy_canary", None);
        let is_replace = matches!(result, LookupResult::Replace(_));
        assert!(is_replace);
    }

    #[test]
    fn applicator_policy_lookup() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        let result = app.lookup_policy("max_retries", None);
        let is_replace = matches!(result, LookupResult::Replace(_));
        assert!(is_replace);
    }

    #[test]
    fn applicator_records_substitution() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let app = OverrideApplicator::new(&pkg);
        app.lookup_pattern("rate_limit_api", Some("orig_h"));
        let subs = app.substitutions();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].item_id, "rate_limit_api");
        assert_eq!(subs[0].category, "pattern");
        assert_eq!(subs[0].action, OverrideAction::Replace);
        assert_eq!(subs[0].original_hash.as_deref(), Some("orig_h"));
    }

    // ── Serde roundtrips ────────────────────────────────────────────────

    #[test]
    fn override_action_serde() {
        for action in [
            OverrideAction::Replace,
            OverrideAction::Disable,
            OverrideAction::Add,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let restored: OverrideAction = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, action);
        }
    }

    #[test]
    fn override_package_json_roundtrip() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let json = serde_json::to_string(&pkg).unwrap();
        let restored: OverridePackage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.meta.name, pkg.meta.name);
        assert_eq!(restored.override_count(), pkg.override_count());
    }

    #[test]
    fn override_error_display() {
        let err = OverrideError::UnknownRuleId("foo".into());
        assert!(err.to_string().contains("foo"));
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let mut baseline = BTreeMap::new();
        baseline.insert("rate_limit_api".to_string(), "h1".to_string());
        baseline.insert("error_detect_timeout".to_string(), "h2".to_string());
        baseline.insert("deploy_canary".to_string(), "h3".to_string());
        baseline.insert("max_retries".to_string(), "h4".to_string());
        let manifest = OverrideManifest::build(&pkg, &baseline);
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: OverrideManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.entries.len(), manifest.entries.len());
    }

    #[test]
    fn substitution_record_serde() {
        let rec = SubstitutionRecord {
            item_id: "foo".into(),
            category: "pattern".into(),
            action: OverrideAction::Replace,
            original_hash: Some("h1".into()),
            override_hash: Some("h2".into()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        let restored: SubstitutionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.item_id, "foo");
    }

    #[test]
    fn all_ids_collects_everything() {
        let pkg = OverridePackageLoader::load(sample_toml()).unwrap();
        let ids = pkg.all_ids();
        assert_eq!(ids.len(), 4);
        assert!(ids.contains(&"rate_limit_api".to_string()));
        assert!(ids.contains(&"deploy_canary".to_string()));
    }

    fn resource_override_toml() -> &'static str {
        r#"
[meta]
schema_version = "ft.resource_control_override.v1"
name = "resource-candidate"
description = "tighten queue pressure while raising hot memory budget"
base_trace = "baseline.fttrace"
created_at = "2026-05-06T00:00:00Z"
author = "SilverHarbor"

[[overrides]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "0.82"
reason = "degrade before p99 latency spikes"

[[overrides]]
knob_id = "memory.hot_resident_budget_bytes"
domain = "memory_tier"
value = "268435456"
reason = "give hot scrollback more room during replay"

[[overrides]]
knob_id = "autotune.enabled"
domain = "auto_tune"
action = "disable"
reason = "hold auto-tune constant for baseline comparison"
"#
    }

    fn sectioned_resource_override_toml() -> &'static str {
        r#"
[meta]
schema_version = "ft.resource_control_override.v1"
name = "sectioned-resource-candidate"

[[admission]]
knob_id = "admission.max_pending_items"
domain = "admission"
value = "2048"
reason = "bound queue growth"

[[qos]]
knob_id = "qos.interactive_weight"
domain = "qos"
value = "2.5"
reason = "favor live panes"

[[topology]]
knob_id = "topology.locality_spread_factor"
domain = "topology"
value = "0.75"
reason = "spread pressure across hosts"

[[memory_tier]]
knob_id = "memory.search_cache_budget_bytes"
domain = "memory_tier"
value = "134217728"
reason = "cap search cache footprint"

[[auto_tune]]
knob_id = "autotune.exploration_budget_percent"
domain = "auto_tune"
value = "5.0"
reason = "limit replay candidate exploration"
"#
    }

    const RESOURCE_TEST_KNOBS: [(&str, &str, &str); 5] = [
        ("admission.max_queue_utilization", "admission", "0.80"),
        ("qos.interactive_weight", "qos", "1.5"),
        ("topology.max_migrations_per_epoch", "topology", "12"),
        (
            "memory.hot_resident_budget_bytes",
            "memory_tier",
            "67108864",
        ),
        ("autotune.enabled", "auto_tune", "true"),
    ];

    fn resource_override_toml_from_indices(indices: &[usize]) -> String {
        let mut toml = String::from(
            r#"
[meta]
name = "generated-resource-candidate"
"#,
        );
        for index in indices {
            let (knob_id, domain, value) = RESOURCE_TEST_KNOBS[*index];
            toml.push_str(&format!(
                r#"
[[overrides]]
knob_id = "{knob_id}"
domain = "{domain}"
value = "{value}"
reason = "generated"
"#
            ));
        }
        toml
    }

    // ── Resource-control override packages ─────────────────────────────

    #[test]
    fn resource_override_package_parses_and_hashes() {
        let pkg = ResourceControlOverrideLoader::load(resource_override_toml()).unwrap();
        assert_eq!(pkg.meta.name, "resource-candidate");
        assert_eq!(pkg.override_count(), 3);
        assert!(
            pkg.overrides
                .iter()
                .all(|override_| override_.value_hash.is_some())
        );
        let ids = pkg.knob_ids();
        assert_eq!(
            ids,
            vec![
                "admission.max_queue_utilization".to_string(),
                "autotune.enabled".to_string(),
                "memory.hot_resident_budget_bytes".to_string(),
            ]
        );
    }

    #[test]
    fn resource_override_sectioned_package_covers_domains() {
        let pkg = ResourceControlOverrideLoader::load(sectioned_resource_override_toml()).unwrap();
        assert_eq!(pkg.override_count(), 5);
        assert_eq!(pkg.admission.len(), 1);
        assert_eq!(pkg.qos.len(), 1);
        assert_eq!(pkg.topology.len(), 1);
        assert_eq!(pkg.memory_tier.len(), 1);
        assert_eq!(pkg.auto_tune.len(), 1);
        assert_eq!(
            pkg.knob_ids(),
            vec![
                "admission.max_pending_items".to_string(),
                "autotune.exploration_budget_percent".to_string(),
                "memory.search_cache_budget_bytes".to_string(),
                "qos.interactive_weight".to_string(),
                "topology.locality_spread_factor".to_string(),
            ]
        );
    }

    #[test]
    fn resource_override_rejects_section_domain_mismatch() {
        let toml = r#"
[meta]
name = "bad-section"

[[memory_tier]]
knob_id = "memory.hot_resident_budget_bytes"
domain = "admission"
value = "268435456"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(
            error,
            ResourceOverrideError::DomainMismatch { .. }
        ));
    }

    #[test]
    fn resource_override_rejects_unknown_knob() {
        let toml = r#"
[meta]
name = "unknown"

[[overrides]]
knob_id = "scheduler.mystery_knob"
domain = "admission"
value = "1"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(error, ResourceOverrideError::UnknownKnob(_)));
    }

    #[test]
    fn resource_override_rejects_wildcard_knob() {
        let toml = r#"
[meta]
name = "wildcard"

[[overrides]]
knob_id = "admission.*"
domain = "admission"
value = "0.5"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(error, ResourceOverrideError::UnsafeWildcard(_)));
    }

    #[test]
    fn resource_override_rejects_domain_mismatch() {
        let toml = r#"
[meta]
name = "domain-mismatch"

[[overrides]]
knob_id = "memory.hot_resident_budget_bytes"
domain = "admission"
value = "268435456"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(
            error,
            ResourceOverrideError::DomainMismatch { .. }
        ));
    }

    #[test]
    fn resource_override_rejects_out_of_range_value() {
        let toml = r#"
[meta]
name = "out-of-range"

[[overrides]]
knob_id = "admission.max_queue_utilization"
domain = "admission"
value = "1.5"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(
            error,
            ResourceOverrideError::ValueOutOfRange { .. }
        ));
    }

    #[test]
    fn resource_override_rejects_bad_value_type() {
        let toml = r#"
[meta]
name = "bad-type"

[[overrides]]
knob_id = "autotune.enabled"
domain = "auto_tune"
value = "sometimes"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(
            error,
            ResourceOverrideError::ValueTypeMismatch { .. }
        ));
    }

    #[test]
    fn resource_override_rejects_duplicate_knob() {
        let toml = r#"
[meta]
name = "duplicate"

[[overrides]]
knob_id = "qos.interactive_weight"
domain = "qos"
value = "2.0"

[[overrides]]
knob_id = "qos.interactive_weight"
domain = "qos"
value = "3.0"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(
            error,
            ResourceOverrideError::ConflictingKnobOverrides(_)
        ));
    }

    #[test]
    fn resource_override_rejects_disable_when_not_allowed() {
        let toml = r#"
[meta]
name = "unsafe-disable"

[[overrides]]
knob_id = "admission.max_pending_items"
domain = "admission"
action = "disable"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(error, ResourceOverrideError::DisableNotAllowed(_)));
    }

    #[test]
    fn resource_override_allows_reset_without_value() {
        let toml = r#"
[meta]
name = "reset"

[[overrides]]
knob_id = "topology.max_migrations_per_epoch"
domain = "topology"
action = "reset_to_default"
"#;
        let pkg = ResourceControlOverrideLoader::load(toml).unwrap();
        assert_eq!(pkg.override_count(), 1);
        assert_eq!(
            pkg.overrides[0].action,
            ResourceOverrideAction::ResetToDefault
        );
        assert!(pkg.overrides[0].value_hash.is_some());
    }

    #[test]
    fn resource_override_hash_mismatch_rejected() {
        let toml = r#"
[meta]
name = "hash-mismatch"

[[overrides]]
knob_id = "qos.bulk_search_weight"
domain = "qos"
value = "2.0"
value_hash = "0000000000000000"
"#;
        let error = ResourceControlOverrideLoader::load(toml).unwrap_err();
        assert!(matches!(error, ResourceOverrideError::HashMismatch { .. }));
    }

    #[test]
    fn resource_manifest_is_deterministic_and_sorted() {
        let pkg = ResourceControlOverrideLoader::load(resource_override_toml()).unwrap();
        let mut baseline = BTreeMap::new();
        baseline.insert(
            "memory.hot_resident_budget_bytes".to_string(),
            "baseline-memory".to_string(),
        );
        baseline.insert(
            "admission.max_queue_utilization".to_string(),
            "baseline-admission".to_string(),
        );

        let manifest = ResourceControlOverrideManifest::build(&pkg, &baseline);
        assert_eq!(
            manifest.schema_version,
            RESOURCE_CONTROL_OVERRIDE_SCHEMA_VERSION
        );
        assert_eq!(manifest.entries.len(), 3);
        assert_eq!(
            manifest
                .entries
                .iter()
                .map(|entry| entry.knob_id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "admission.max_queue_utilization",
                "autotune.enabled",
                "memory.hot_resident_budget_bytes",
            ]
        );
        assert_eq!(
            manifest.entries[0].baseline_hash.as_deref(),
            Some("baseline-admission")
        );
        assert!(manifest.entries[1].baseline_hash.is_none());
    }

    #[test]
    fn resource_manifest_serde_roundtrip() {
        let pkg = ResourceControlOverrideLoader::load(resource_override_toml()).unwrap();
        let manifest = ResourceControlOverrideManifest::build(&pkg, &BTreeMap::new());
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: ResourceControlOverrideManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, manifest);
    }

    proptest! {
        #[test]
        fn resource_manifest_order_is_deterministic_for_any_input_order(
            raw_indices in proptest::collection::vec(0usize..RESOURCE_TEST_KNOBS.len(), 1..32)
        ) {
            let mut seen = BTreeSet::new();
            let unique = raw_indices
                .into_iter()
                .filter(|index| seen.insert(*index))
                .collect::<Vec<_>>();
            let toml = resource_override_toml_from_indices(&unique);
            let pkg = ResourceControlOverrideLoader::load(&toml).unwrap();
            let manifest = ResourceControlOverrideManifest::build(&pkg, &BTreeMap::new());
            let ids = manifest
                .entries
                .iter()
                .map(|entry| entry.knob_id.as_str())
                .collect::<Vec<_>>();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();

            prop_assert_eq!(ids, sorted);
        }

        #[test]
        fn resource_duplicate_knobs_fail_closed(knob_index in 0usize..RESOURCE_TEST_KNOBS.len()) {
            let toml = resource_override_toml_from_indices(&[knob_index, knob_index]);
            let error = ResourceControlOverrideLoader::load(&toml).unwrap_err();
            prop_assert!(matches!(
                error,
                ResourceOverrideError::ConflictingKnobOverrides(_)
            ));
        }
    }
}
