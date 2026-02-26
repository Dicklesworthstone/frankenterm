//! Counterfactual override package format, loader, and applicator (ft-og6q6.4.1).
//!
//! Provides:
//! - [`OverridePackage`] — Declarative what-if experiment manifest (.ftoverride format).
//! - [`OverridePackageLoader`] — Validates and loads override packages from TOML.
//! - [`OverrideApplicator`] — Matches rule IDs to overrides at decision time.
//! - [`OverrideManifest`] — Hash-pair list for diff detection.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
        assert!(manifest
            .entries
            .iter()
            .all(|e| e.action == OverrideAction::Disable));
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
}
