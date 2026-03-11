//! Multi-tenant namespace isolation and cross-tenant guardrails (ft-3681t.6.6).
//!
//! Formalizes tenant/workspace isolation boundaries for agents, sessions,
//! workflows, and connectors so cross-tenant leakage or action bleed-through
//! is prevented by construction.
//!
//! # Architecture
//!
//! Each resource (pane, session, workflow, connector) is assigned to exactly one
//! [`TenantNamespace`]. Cross-namespace access is governed by
//! [`CrossTenantPolicy`] which defaults to **deny** — making isolation the
//! zero-configuration default.
//!
//! # Namespace Hierarchy
//!
//! Namespaces use a dot-separated hierarchical scheme:
//! - `"default"` — the implicit namespace when none is specified.
//! - `"org.team"` — two-level hierarchy.
//! - `"org.team.project"` — three-level hierarchy.
//!
//! A principal in `"org.team"` does **not** automatically gain access to
//! `"org.team.project"` unless the [`CrossTenantPolicy`] explicitly allows
//! hierarchical delegation.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

// =============================================================================
// Tenant namespace identifier
// =============================================================================

/// A typed namespace identifier with hierarchical naming.
///
/// Namespaces are dot-separated strings (e.g., `"org.team.project"`).
/// The empty string is not valid; use `TenantNamespace::default()` which
/// returns the `"default"` namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TenantNamespace(String);

impl TenantNamespace {
    /// The well-known default namespace assigned when none is specified.
    pub const DEFAULT_NAME: &'static str = "default";

    /// The well-known system namespace for internal/infrastructure resources.
    pub const SYSTEM_NAME: &'static str = "system";

    /// Maximum depth of namespace hierarchy (segments separated by `.`).
    pub const MAX_DEPTH: usize = 8;

    /// Maximum length of the full namespace string.
    pub const MAX_LEN: usize = 256;

    /// Creates a new namespace, validating the name.
    ///
    /// Returns `None` if the name is empty, exceeds [`Self::MAX_LEN`],
    /// exceeds [`Self::MAX_DEPTH`] segments, or contains invalid characters.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        if !Self::is_valid_name(&name) {
            return None;
        }
        Some(Self(name))
    }

    /// Creates a namespace without validation. Caller must ensure validity.
    ///
    /// # Panics
    ///
    /// Panics in debug mode if the name is invalid.
    #[must_use]
    pub fn new_unchecked(name: impl Into<String>) -> Self {
        let name = name.into();
        debug_assert!(Self::is_valid_name(&name), "invalid namespace name: {name}");
        Self(name)
    }

    /// Returns the default namespace.
    #[must_use]
    pub fn default_ns() -> Self {
        Self(Self::DEFAULT_NAME.to_owned())
    }

    /// Returns the system namespace.
    #[must_use]
    pub fn system() -> Self {
        Self(Self::SYSTEM_NAME.to_owned())
    }

    /// Returns the full namespace string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the number of hierarchy segments.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.0.split('.').count()
    }

    /// Returns the parent namespace, or `None` if this is a top-level namespace.
    ///
    /// Example: `"org.team.project".parent()` → `Some("org.team")`.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        let name = &self.0;
        name.rfind('.').map(|pos| Self(name[..pos].to_owned()))
    }

    /// Returns `true` if `other` is an ancestor of `self` in the hierarchy.
    ///
    /// Example: `"org.team.project".is_descendant_of("org.team")` → `true`.
    #[must_use]
    pub fn is_descendant_of(&self, other: &Self) -> bool {
        if self.0.len() <= other.0.len() {
            return false;
        }
        self.0.starts_with(&other.0) && self.0.as_bytes().get(other.0.len()) == Some(&b'.')
    }

    /// Returns `true` if `other` is a descendant of `self`.
    #[must_use]
    pub fn is_ancestor_of(&self, other: &Self) -> bool {
        other.is_descendant_of(self)
    }

    /// Returns `true` if this is the default namespace.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT_NAME
    }

    /// Returns `true` if this is the system namespace.
    #[must_use]
    pub fn is_system(&self) -> bool {
        self.0 == Self::SYSTEM_NAME
    }

    /// Iterates over all ancestor namespaces from immediate parent to root.
    pub fn ancestors(&self) -> impl Iterator<Item = Self> + '_ {
        let mut current = self.0.as_str();
        std::iter::from_fn(move || {
            let pos = current.rfind('.')?;
            current = &current[..pos];
            Some(Self(current.to_owned()))
        })
    }

    /// Validates a namespace name.
    fn is_valid_name(name: &str) -> bool {
        if name.is_empty() || name.len() > Self::MAX_LEN {
            return false;
        }
        let segments: Vec<&str> = name.split('.').collect();
        if segments.len() > Self::MAX_DEPTH {
            return false;
        }
        segments.iter().all(|seg| {
            !seg.is_empty()
                && seg.len() <= 64
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    }
}

impl Default for TenantNamespace {
    fn default() -> Self {
        Self::default_ns()
    }
}

impl std::fmt::Display for TenantNamespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// =============================================================================
// Resource binding
// =============================================================================

/// Kind of resource bound to a namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespacedResourceKind {
    /// A terminal pane.
    Pane,
    /// A terminal session (group of panes).
    Session,
    /// A workflow definition or execution.
    Workflow,
    /// An external connector instance.
    Connector,
    /// An agent identity.
    Agent,
    /// A credential or secret.
    Credential,
}

impl NamespacedResourceKind {
    /// Returns the string tag for this kind.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Session => "session",
            Self::Workflow => "workflow",
            Self::Connector => "connector",
            Self::Agent => "agent",
            Self::Credential => "credential",
        }
    }
}

/// A binding of a specific resource instance to a namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NamespaceBinding {
    /// The namespace this resource belongs to.
    pub namespace: TenantNamespace,
    /// The kind of resource.
    pub resource_kind: NamespacedResourceKind,
    /// The resource identifier (e.g., pane ID as string, workflow name).
    pub resource_id: String,
}

impl NamespaceBinding {
    /// Creates a new namespace binding.
    #[must_use]
    pub fn new(
        namespace: TenantNamespace,
        resource_kind: NamespacedResourceKind,
        resource_id: impl Into<String>,
    ) -> Self {
        Self {
            namespace,
            resource_kind,
            resource_id: resource_id.into(),
        }
    }

    /// Returns a stable key for indexing.
    #[must_use]
    pub fn stable_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.resource_kind.as_str(),
            self.namespace.as_str(),
            self.resource_id
        )
    }
}

// =============================================================================
// Cross-tenant policy
// =============================================================================

/// The decision for a cross-tenant access attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossTenantDecision {
    /// Access is denied outright.
    Deny,
    /// Access is allowed but audit-logged.
    AllowWithAudit,
    /// Access is allowed silently (no audit entry).
    Allow,
}

impl CrossTenantDecision {
    /// Returns `true` if access is permitted (either `Allow` or `AllowWithAudit`).
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow | Self::AllowWithAudit)
    }

    /// Returns `true` if this decision requires an audit log entry.
    #[must_use]
    pub const fn requires_audit(&self) -> bool {
        matches!(self, Self::AllowWithAudit)
    }
}

/// A rule governing cross-tenant access between two namespaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossTenantRule {
    /// Source namespace (the accessor).
    pub source: TenantNamespace,
    /// Target namespace (the resource owner).
    pub target: TenantNamespace,
    /// Which resource kinds this rule applies to (empty = all kinds).
    pub resource_kinds: BTreeSet<NamespacedResourceKind>,
    /// The decision to apply.
    pub decision: CrossTenantDecision,
    /// Human-readable reason for this rule.
    pub reason: Option<String>,
}

impl CrossTenantRule {
    /// Creates a deny rule between two namespaces.
    #[must_use]
    pub fn deny(source: TenantNamespace, target: TenantNamespace) -> Self {
        Self {
            source,
            target,
            resource_kinds: BTreeSet::new(),
            decision: CrossTenantDecision::Deny,
            reason: None,
        }
    }

    /// Creates an allow-with-audit rule between two namespaces.
    #[must_use]
    pub fn allow_with_audit(source: TenantNamespace, target: TenantNamespace) -> Self {
        Self {
            source,
            target,
            resource_kinds: BTreeSet::new(),
            decision: CrossTenantDecision::AllowWithAudit,
            reason: None,
        }
    }

    /// Adds a reason to this rule.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Limits this rule to specific resource kinds.
    #[must_use]
    pub fn for_kinds(mut self, kinds: impl IntoIterator<Item = NamespacedResourceKind>) -> Self {
        self.resource_kinds = kinds.into_iter().collect();
        self
    }

    /// Returns `true` if this rule applies to the given resource kind.
    #[must_use]
    pub fn applies_to(&self, kind: NamespacedResourceKind) -> bool {
        self.resource_kinds.is_empty() || self.resource_kinds.contains(&kind)
    }
}

/// Configuration for cross-tenant access policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossTenantPolicy {
    /// The default decision when no explicit rule matches.
    /// Defaults to [`CrossTenantDecision::Deny`] (deny-by-default).
    pub default_decision: CrossTenantDecision,

    /// Whether to allow hierarchical access (parent namespace can access
    /// descendant namespaces). Defaults to `false`.
    pub allow_hierarchical: bool,

    /// Whether the system namespace bypasses all cross-tenant checks.
    /// Defaults to `true`.
    pub system_bypass: bool,

    /// Explicit rules that override the default decision.
    pub rules: Vec<CrossTenantRule>,
}

impl Default for CrossTenantPolicy {
    fn default() -> Self {
        Self {
            default_decision: CrossTenantDecision::Deny,
            allow_hierarchical: false,
            system_bypass: true,
            rules: Vec::new(),
        }
    }
}

impl CrossTenantPolicy {
    /// Creates a fully permissive policy (allow all cross-tenant access with audit).
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            default_decision: CrossTenantDecision::AllowWithAudit,
            allow_hierarchical: true,
            system_bypass: true,
            rules: Vec::new(),
        }
    }

    /// Creates a strict policy (deny all cross-tenant access).
    #[must_use]
    pub fn strict() -> Self {
        Self {
            default_decision: CrossTenantDecision::Deny,
            allow_hierarchical: false,
            system_bypass: true,
            rules: Vec::new(),
        }
    }

    /// Adds an explicit cross-tenant rule.
    #[must_use]
    pub fn with_rule(mut self, rule: CrossTenantRule) -> Self {
        self.rules.push(rule);
        self
    }
}

// =============================================================================
// Boundary check result
// =============================================================================

/// The result of a namespace boundary check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryCheckResult {
    /// Whether the access crosses a namespace boundary.
    pub crosses_boundary: bool,
    /// The source namespace of the accessor.
    pub source_namespace: TenantNamespace,
    /// The target namespace of the resource.
    pub target_namespace: TenantNamespace,
    /// The decision applied.
    pub decision: CrossTenantDecision,
    /// The rule that determined the decision, if any.
    pub matched_rule: Option<String>,
    /// Whether hierarchical delegation was applied.
    pub hierarchical_match: bool,
}

impl BoundaryCheckResult {
    /// Returns `true` if the access is allowed.
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        self.decision.is_allowed()
    }

    /// Returns `true` if audit logging is required.
    ///
    /// Both allowed and denied cross-boundary accesses are audited
    /// for forensic and compliance reporting.
    #[must_use]
    pub fn requires_audit(&self) -> bool {
        self.crosses_boundary || self.decision.requires_audit()
    }
}

// =============================================================================
// Namespace registry
// =============================================================================

/// Tracks which resources belong to which namespace.
///
/// Thread-safe: designed for single-writer access. For concurrent use,
/// wrap in an appropriate synchronization primitive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NamespaceRegistry {
    /// Map from (resource_kind, resource_id) → namespace.
    bindings: BTreeMap<(String, String), TenantNamespace>,
    /// Reverse index: namespace → set of (resource_kind, resource_id).
    by_namespace: BTreeMap<TenantNamespace, BTreeSet<(String, String)>>,
    /// Cross-tenant access policy.
    policy: CrossTenantPolicy,
    /// Audit log of cross-tenant access attempts.
    audit_log: Vec<BoundaryAuditEntry>,
    /// Maximum audit log entries before oldest are discarded.
    max_audit_entries: usize,
}

/// An audit log entry for a cross-tenant access attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryAuditEntry {
    /// Timestamp in milliseconds since UNIX epoch.
    pub timestamp_ms: u64,
    /// The accessor's namespace.
    pub source_namespace: TenantNamespace,
    /// The resource's namespace.
    pub target_namespace: TenantNamespace,
    /// The kind of resource accessed.
    pub resource_kind: String,
    /// The resource identifier.
    pub resource_id: String,
    /// The decision applied.
    pub decision: CrossTenantDecision,
    /// The reason for the decision.
    pub reason: Option<String>,
}

impl NamespaceRegistry {
    /// Default maximum audit log entries.
    pub const DEFAULT_MAX_AUDIT: usize = 4096;

    /// Creates a new registry with the default policy (deny cross-tenant).
    #[must_use]
    pub fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
            by_namespace: BTreeMap::new(),
            policy: CrossTenantPolicy::default(),
            audit_log: Vec::new(),
            max_audit_entries: Self::DEFAULT_MAX_AUDIT,
        }
    }

    /// Creates a new registry with a custom policy.
    #[must_use]
    pub fn with_policy(policy: CrossTenantPolicy) -> Self {
        Self {
            policy,
            ..Self::new()
        }
    }

    /// Returns a reference to the current cross-tenant policy.
    #[must_use]
    pub fn policy(&self) -> &CrossTenantPolicy {
        &self.policy
    }

    /// Updates the cross-tenant policy.
    pub fn set_policy(&mut self, policy: CrossTenantPolicy) {
        self.policy = policy;
    }

    // ---- Binding management ----

    /// Binds a resource to a namespace.
    ///
    /// If the resource was already bound to a different namespace, the old
    /// binding is replaced and the previous namespace is returned.
    pub fn bind(
        &mut self,
        kind: NamespacedResourceKind,
        resource_id: impl Into<String>,
        namespace: TenantNamespace,
    ) -> Option<TenantNamespace> {
        let resource_id = resource_id.into();
        let key = (kind.as_str().to_owned(), resource_id.clone());
        let prev = self.bindings.insert(key.clone(), namespace.clone());

        // Update reverse index: remove from old namespace if different.
        if let Some(ref old_ns) = prev {
            if old_ns != &namespace {
                if let Some(set) = self.by_namespace.get_mut(old_ns) {
                    set.remove(&key);
                    if set.is_empty() {
                        self.by_namespace.remove(old_ns);
                    }
                }
            }
        }

        // Add to new namespace reverse index.
        self.by_namespace.entry(namespace).or_default().insert(key);

        prev
    }

    /// Removes the namespace binding for a resource.
    ///
    /// Returns the namespace it was bound to, if any.
    pub fn unbind(
        &mut self,
        kind: NamespacedResourceKind,
        resource_id: &str,
    ) -> Option<TenantNamespace> {
        let key = (kind.as_str().to_owned(), resource_id.to_owned());
        let ns = self.bindings.remove(&key)?;

        if let Some(set) = self.by_namespace.get_mut(&ns) {
            set.remove(&key);
            if set.is_empty() {
                self.by_namespace.remove(&ns);
            }
        }

        Some(ns)
    }

    /// Looks up the namespace for a given resource.
    ///
    /// Returns the default namespace if the resource has no explicit binding.
    #[must_use]
    pub fn lookup(&self, kind: NamespacedResourceKind, resource_id: &str) -> TenantNamespace {
        let key = (kind.as_str().to_owned(), resource_id.to_owned());
        self.bindings.get(&key).cloned().unwrap_or_default()
    }

    /// Returns `true` if the resource has an explicit namespace binding.
    #[must_use]
    pub fn is_bound(&self, kind: NamespacedResourceKind, resource_id: &str) -> bool {
        let key = (kind.as_str().to_owned(), resource_id.to_owned());
        self.bindings.contains_key(&key)
    }

    /// Returns all resources bound to a given namespace.
    #[must_use]
    pub fn resources_in_namespace(
        &self,
        namespace: &TenantNamespace,
    ) -> Vec<(NamespacedResourceKind, String)> {
        self.by_namespace
            .get(namespace)
            .map(|set| {
                set.iter()
                    .filter_map(|(kind_str, id)| {
                        let kind = match kind_str.as_str() {
                            "pane" => NamespacedResourceKind::Pane,
                            "session" => NamespacedResourceKind::Session,
                            "workflow" => NamespacedResourceKind::Workflow,
                            "connector" => NamespacedResourceKind::Connector,
                            "agent" => NamespacedResourceKind::Agent,
                            "credential" => NamespacedResourceKind::Credential,
                            _ => return None,
                        };
                        Some((kind, id.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the set of distinct namespaces that have at least one binding.
    #[must_use]
    pub fn active_namespaces(&self) -> BTreeSet<TenantNamespace> {
        self.by_namespace.keys().cloned().collect()
    }

    /// Returns the total number of resource bindings.
    #[must_use]
    pub fn binding_count(&self) -> usize {
        self.bindings.len()
    }

    // ---- Boundary enforcement ----

    /// Checks whether access from `source_ns` to a resource in `target_ns`
    /// is allowed under the current cross-tenant policy.
    #[must_use]
    pub fn check_boundary(
        &self,
        source_ns: &TenantNamespace,
        target_ns: &TenantNamespace,
        resource_kind: NamespacedResourceKind,
    ) -> BoundaryCheckResult {
        // Same-namespace access is always allowed.
        if source_ns == target_ns {
            return BoundaryCheckResult {
                crosses_boundary: false,
                source_namespace: source_ns.clone(),
                target_namespace: target_ns.clone(),
                decision: CrossTenantDecision::Allow,
                matched_rule: None,
                hierarchical_match: false,
            };
        }

        // System namespace bypass.
        if self.policy.system_bypass && source_ns.is_system() {
            return BoundaryCheckResult {
                crosses_boundary: true,
                source_namespace: source_ns.clone(),
                target_namespace: target_ns.clone(),
                decision: CrossTenantDecision::AllowWithAudit,
                matched_rule: Some("system_bypass".to_owned()),
                hierarchical_match: false,
            };
        }

        // Hierarchical delegation check.
        if self.policy.allow_hierarchical && target_ns.is_descendant_of(source_ns) {
            return BoundaryCheckResult {
                crosses_boundary: true,
                source_namespace: source_ns.clone(),
                target_namespace: target_ns.clone(),
                decision: CrossTenantDecision::AllowWithAudit,
                matched_rule: Some("hierarchical_delegation".to_owned()),
                hierarchical_match: true,
            };
        }

        // Check explicit rules (first match wins).
        for (i, rule) in self.policy.rules.iter().enumerate() {
            if rule.source == *source_ns
                && rule.target == *target_ns
                && rule.applies_to(resource_kind)
            {
                return BoundaryCheckResult {
                    crosses_boundary: true,
                    source_namespace: source_ns.clone(),
                    target_namespace: target_ns.clone(),
                    decision: rule.decision,
                    matched_rule: Some(format!(
                        "rule[{}]{}",
                        i,
                        rule.reason
                            .as_deref()
                            .map(|r| format!(": {r}"))
                            .unwrap_or_default()
                    )),
                    hierarchical_match: false,
                };
            }
        }

        // Default decision.
        BoundaryCheckResult {
            crosses_boundary: true,
            source_namespace: source_ns.clone(),
            target_namespace: target_ns.clone(),
            decision: self.policy.default_decision,
            matched_rule: None,
            hierarchical_match: false,
        }
    }

    /// Checks and records a cross-tenant access attempt.
    ///
    /// Like [`check_boundary`](Self::check_boundary), but also appends an
    /// audit log entry when the decision requires auditing.
    pub fn check_and_audit(
        &mut self,
        source_ns: &TenantNamespace,
        target_ns: &TenantNamespace,
        resource_kind: NamespacedResourceKind,
        resource_id: &str,
        timestamp_ms: u64,
    ) -> BoundaryCheckResult {
        let result = self.check_boundary(source_ns, target_ns, resource_kind);

        if result.requires_audit() {
            let entry = BoundaryAuditEntry {
                timestamp_ms,
                source_namespace: source_ns.clone(),
                target_namespace: target_ns.clone(),
                resource_kind: resource_kind.as_str().to_owned(),
                resource_id: resource_id.to_owned(),
                decision: result.decision,
                reason: result.matched_rule.clone(),
            };
            self.audit_log.push(entry);

            // Evict oldest entries if over limit.
            if self.audit_log.len() > self.max_audit_entries {
                let excess = self.audit_log.len() - self.max_audit_entries;
                self.audit_log.drain(..excess);
            }
        }

        result
    }

    /// Returns a snapshot of the audit log.
    #[must_use]
    pub fn audit_log(&self) -> &[BoundaryAuditEntry] {
        &self.audit_log
    }

    /// Clears the audit log.
    pub fn clear_audit_log(&mut self) {
        self.audit_log.clear();
    }

    /// Returns a snapshot of the registry state for diagnostics.
    #[must_use]
    pub fn snapshot(&self) -> NamespaceRegistrySnapshot {
        let mut namespace_counts: HashMap<String, usize> = HashMap::new();
        for ns in self.by_namespace.keys() {
            let count = self.by_namespace.get(ns).map_or(0, BTreeSet::len);
            namespace_counts.insert(ns.as_str().to_owned(), count);
        }

        NamespaceRegistrySnapshot {
            total_bindings: self.bindings.len(),
            active_namespaces: self.by_namespace.len(),
            namespace_counts,
            audit_log_size: self.audit_log.len(),
            policy_rule_count: self.policy.rules.len(),
            default_decision: self.policy.default_decision,
        }
    }
}

/// A diagnostic snapshot of the namespace registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceRegistrySnapshot {
    /// Total resource bindings.
    pub total_bindings: usize,
    /// Number of active (non-empty) namespaces.
    pub active_namespaces: usize,
    /// Bindings per namespace.
    pub namespace_counts: HashMap<String, usize>,
    /// Current audit log size.
    pub audit_log_size: usize,
    /// Number of explicit cross-tenant rules.
    pub policy_rule_count: usize,
    /// The default cross-tenant decision.
    pub default_decision: CrossTenantDecision,
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for namespace isolation within the policy engine.
///
/// Wraps [`CrossTenantPolicy`] and operational tuning parameters so the
/// namespace isolation subsystem can be configured via TOML/`SafetyConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceIsolationConfig {
    /// Whether namespace isolation enforcement is enabled.
    /// When `false`, the namespace registry still tracks bindings but
    /// boundary checks always return `Allow`.
    pub enabled: bool,

    /// Cross-tenant access policy (default decision, hierarchical delegation,
    /// system bypass, explicit rules).
    pub cross_tenant_policy: CrossTenantPolicy,

    /// Maximum audit log entries before oldest are discarded.
    pub max_audit_entries: usize,
}

impl Default for NamespaceIsolationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cross_tenant_policy: CrossTenantPolicy::default(),
            max_audit_entries: NamespaceRegistry::DEFAULT_MAX_AUDIT,
        }
    }
}

impl NamespaceIsolationConfig {
    /// Creates a permissive config (allow all cross-tenant with audit).
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::permissive(),
            max_audit_entries: NamespaceRegistry::DEFAULT_MAX_AUDIT,
        }
    }

    /// Creates a strict config (deny all cross-tenant, no hierarchical).
    #[must_use]
    pub fn strict() -> Self {
        Self {
            enabled: true,
            cross_tenant_policy: CrossTenantPolicy::strict(),
            max_audit_entries: NamespaceRegistry::DEFAULT_MAX_AUDIT,
        }
    }

    /// Creates a disabled config (no enforcement).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

impl NamespaceRegistry {
    /// Creates a registry from a [`NamespaceIsolationConfig`].
    #[must_use]
    pub fn from_config(config: &NamespaceIsolationConfig) -> Self {
        Self {
            policy: config.cross_tenant_policy.clone(),
            max_audit_entries: config.max_audit_entries,
            ..Self::new()
        }
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for namespace isolation operations.
#[derive(Debug, Default)]
pub struct NamespaceIsolationTelemetry {
    /// Total boundary checks performed.
    pub checks_total: u64,
    /// Checks that crossed a namespace boundary.
    pub cross_boundary_total: u64,
    /// Cross-boundary checks that were denied.
    pub cross_boundary_denied: u64,
    /// Cross-boundary checks that were allowed with audit.
    pub cross_boundary_audited: u64,
    /// Cross-boundary checks allowed via hierarchical delegation.
    pub hierarchical_grants: u64,
    /// Cross-boundary checks allowed via system bypass.
    pub system_bypass_grants: u64,
}

/// Serializable snapshot of telemetry counters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceIsolationTelemetrySnapshot {
    pub checks_total: u64,
    pub cross_boundary_total: u64,
    pub cross_boundary_denied: u64,
    pub cross_boundary_audited: u64,
    pub hierarchical_grants: u64,
    pub system_bypass_grants: u64,
}

impl NamespaceIsolationTelemetry {
    /// Records a boundary check result.
    pub fn record(&mut self, result: &BoundaryCheckResult) {
        self.checks_total += 1;
        if result.crosses_boundary {
            self.cross_boundary_total += 1;
            match result.decision {
                CrossTenantDecision::Deny => self.cross_boundary_denied += 1,
                CrossTenantDecision::AllowWithAudit => self.cross_boundary_audited += 1,
                CrossTenantDecision::Allow => {}
            }
            if result.hierarchical_match {
                self.hierarchical_grants += 1;
            }
            if result
                .matched_rule
                .as_deref()
                .is_some_and(|r| r == "system_bypass")
            {
                self.system_bypass_grants += 1;
            }
        }
    }

    /// Takes a snapshot of the current counters.
    #[must_use]
    pub fn snapshot(&self) -> NamespaceIsolationTelemetrySnapshot {
        NamespaceIsolationTelemetrySnapshot {
            checks_total: self.checks_total,
            cross_boundary_total: self.cross_boundary_total,
            cross_boundary_denied: self.cross_boundary_denied,
            cross_boundary_audited: self.cross_boundary_audited,
            hierarchical_grants: self.hierarchical_grants,
            system_bypass_grants: self.system_bypass_grants,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TenantNamespace tests ----

    #[test]
    fn default_namespace() {
        let ns = TenantNamespace::default();
        assert_eq!(ns.as_str(), "default");
        assert!(ns.is_default());
        assert!(!ns.is_system());
        assert_eq!(ns.depth(), 1);
        assert!(ns.parent().is_none());
    }

    #[test]
    fn system_namespace() {
        let ns = TenantNamespace::system();
        assert_eq!(ns.as_str(), "system");
        assert!(ns.is_system());
        assert!(!ns.is_default());
    }

    #[test]
    fn valid_namespace_names() {
        assert!(TenantNamespace::new("org").is_some());
        assert!(TenantNamespace::new("org.team").is_some());
        assert!(TenantNamespace::new("org.team.project").is_some());
        assert!(TenantNamespace::new("my-org").is_some());
        assert!(TenantNamespace::new("my_org").is_some());
        assert!(TenantNamespace::new("a1.b2.c3").is_some());
    }

    #[test]
    fn invalid_namespace_names() {
        assert!(TenantNamespace::new("").is_none());
        assert!(TenantNamespace::new(".").is_none());
        assert!(TenantNamespace::new("org.").is_none());
        assert!(TenantNamespace::new(".org").is_none());
        assert!(TenantNamespace::new("org..team").is_none());
        assert!(TenantNamespace::new("org.team!").is_none());
        assert!(TenantNamespace::new("org team").is_none());

        // Exceeds max depth
        let deep = (0..=TenantNamespace::MAX_DEPTH)
            .map(|i| format!("s{i}"))
            .collect::<Vec<_>>()
            .join(".");
        assert!(TenantNamespace::new(deep).is_none());

        // Exceeds max length
        let long = "a".repeat(TenantNamespace::MAX_LEN + 1);
        assert!(TenantNamespace::new(long).is_none());
    }

    #[test]
    fn namespace_hierarchy() {
        let org = TenantNamespace::new("org").unwrap();
        let team = TenantNamespace::new("org.team").unwrap();
        let project = TenantNamespace::new("org.team.project").unwrap();

        assert_eq!(team.parent(), Some(org.clone()));
        assert_eq!(project.parent(), Some(team.clone()));
        assert!(org.parent().is_none());

        assert!(project.is_descendant_of(&team));
        assert!(project.is_descendant_of(&org));
        assert!(team.is_descendant_of(&org));
        assert!(!org.is_descendant_of(&team));
        assert!(!team.is_descendant_of(&project));
        assert!(!org.is_descendant_of(&org)); // not a descendant of itself
    }

    #[test]
    fn namespace_ancestors() {
        let ns = TenantNamespace::new("a.b.c.d").unwrap();
        let ancestors: Vec<String> = ns.ancestors().map(|a| a.as_str().to_owned()).collect();
        assert_eq!(ancestors, vec!["a.b.c", "a.b", "a"]);
    }

    #[test]
    fn is_descendant_requires_dot_boundary() {
        let org = TenantNamespace::new("org").unwrap();
        let orgx = TenantNamespace::new("orgx").unwrap();
        assert!(!orgx.is_descendant_of(&org));
    }

    #[test]
    fn is_ancestor_inverse() {
        let parent = TenantNamespace::new("org").unwrap();
        let child = TenantNamespace::new("org.team").unwrap();
        assert!(parent.is_ancestor_of(&child));
        assert!(!child.is_ancestor_of(&parent));
    }

    // ---- Serde roundtrip tests ----

    #[test]
    fn tenant_namespace_serde_roundtrip() {
        let ns = TenantNamespace::new("org.team.project").unwrap();
        let json = serde_json::to_string(&ns).unwrap();
        let back: TenantNamespace = serde_json::from_str(&json).unwrap();
        assert_eq!(ns, back);
    }

    #[test]
    fn cross_tenant_decision_serde_roundtrip() {
        for decision in [
            CrossTenantDecision::Deny,
            CrossTenantDecision::AllowWithAudit,
            CrossTenantDecision::Allow,
        ] {
            let json = serde_json::to_string(&decision).unwrap();
            let back: CrossTenantDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(decision, back);
        }
    }

    #[test]
    fn namespace_binding_serde_roundtrip() {
        let binding = NamespaceBinding::new(
            TenantNamespace::new("org.team").unwrap(),
            NamespacedResourceKind::Pane,
            "42",
        );
        let json = serde_json::to_string(&binding).unwrap();
        let back: NamespaceBinding = serde_json::from_str(&json).unwrap();
        assert_eq!(binding, back);
    }

    #[test]
    fn cross_tenant_policy_serde_roundtrip() {
        let policy = CrossTenantPolicy::default().with_rule(
            CrossTenantRule::allow_with_audit(
                TenantNamespace::new("a").unwrap(),
                TenantNamespace::new("b").unwrap(),
            )
            .with_reason("shared project"),
        );
        let json = serde_json::to_string(&policy).unwrap();
        let back: CrossTenantPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, back);
    }

    #[test]
    fn boundary_check_result_serde_roundtrip() {
        let result = BoundaryCheckResult {
            crosses_boundary: true,
            source_namespace: TenantNamespace::new("org-a").unwrap(),
            target_namespace: TenantNamespace::new("org-b").unwrap(),
            decision: CrossTenantDecision::Deny,
            matched_rule: Some("rule[0]: test".to_owned()),
            hierarchical_match: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BoundaryCheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, back);
    }

    #[test]
    fn registry_snapshot_serde_roundtrip() {
        let snap = NamespaceRegistrySnapshot {
            total_bindings: 5,
            active_namespaces: 2,
            namespace_counts: HashMap::from([("default".to_owned(), 3), ("org".to_owned(), 2)]),
            audit_log_size: 10,
            policy_rule_count: 1,
            default_decision: CrossTenantDecision::Deny,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: NamespaceRegistrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let snap = NamespaceIsolationTelemetrySnapshot {
            checks_total: 100,
            cross_boundary_total: 20,
            cross_boundary_denied: 15,
            cross_boundary_audited: 5,
            hierarchical_grants: 2,
            system_bypass_grants: 3,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: NamespaceIsolationTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- NamespaceRegistry tests ----

    #[test]
    fn bind_and_lookup() {
        let mut reg = NamespaceRegistry::new();
        let ns = TenantNamespace::new("org.team").unwrap();

        reg.bind(NamespacedResourceKind::Pane, "42", ns.clone());
        assert_eq!(reg.lookup(NamespacedResourceKind::Pane, "42"), ns);
        assert!(reg.is_bound(NamespacedResourceKind::Pane, "42"));
    }

    #[test]
    fn unbound_resource_returns_default() {
        let reg = NamespaceRegistry::new();
        assert_eq!(
            reg.lookup(NamespacedResourceKind::Pane, "999"),
            TenantNamespace::default()
        );
        assert!(!reg.is_bound(NamespacedResourceKind::Pane, "999"));
    }

    #[test]
    fn rebind_returns_previous() {
        let mut reg = NamespaceRegistry::new();
        let ns1 = TenantNamespace::new("org-a").unwrap();
        let ns2 = TenantNamespace::new("org-b").unwrap();

        assert!(
            reg.bind(NamespacedResourceKind::Agent, "bot", ns1.clone())
                .is_none()
        );
        let prev = reg.bind(NamespacedResourceKind::Agent, "bot", ns2.clone());
        assert_eq!(prev, Some(ns1));
        assert_eq!(reg.lookup(NamespacedResourceKind::Agent, "bot"), ns2);
    }

    #[test]
    fn unbind_removes_binding() {
        let mut reg = NamespaceRegistry::new();
        let ns = TenantNamespace::new("org").unwrap();

        reg.bind(NamespacedResourceKind::Session, "s1", ns.clone());
        let removed = reg.unbind(NamespacedResourceKind::Session, "s1");
        assert_eq!(removed, Some(ns));
        assert!(!reg.is_bound(NamespacedResourceKind::Session, "s1"));
        assert_eq!(reg.binding_count(), 0);
    }

    #[test]
    fn resources_in_namespace() {
        let mut reg = NamespaceRegistry::new();
        let ns = TenantNamespace::new("team").unwrap();

        reg.bind(NamespacedResourceKind::Pane, "1", ns.clone());
        reg.bind(NamespacedResourceKind::Pane, "2", ns.clone());
        reg.bind(NamespacedResourceKind::Workflow, "wf1", ns.clone());

        let resources = reg.resources_in_namespace(&ns);
        assert_eq!(resources.len(), 3);
    }

    #[test]
    fn active_namespaces() {
        let mut reg = NamespaceRegistry::new();
        let ns_a = TenantNamespace::new("a").unwrap();
        let ns_b = TenantNamespace::new("b").unwrap();

        reg.bind(NamespacedResourceKind::Pane, "1", ns_a.clone());
        reg.bind(NamespacedResourceKind::Pane, "2", ns_b.clone());

        let active = reg.active_namespaces();
        assert_eq!(active.len(), 2);
        assert!(active.contains(&ns_a));
        assert!(active.contains(&ns_b));
    }

    // ---- Boundary check tests ----

    #[test]
    fn same_namespace_always_allowed() {
        let reg = NamespaceRegistry::new();
        let ns = TenantNamespace::new("org").unwrap();
        let result = reg.check_boundary(&ns, &ns, NamespacedResourceKind::Pane);
        assert!(result.is_allowed());
        assert!(!result.crosses_boundary);
    }

    #[test]
    fn cross_namespace_denied_by_default() {
        let reg = NamespaceRegistry::new();
        let ns_a = TenantNamespace::new("org-a").unwrap();
        let ns_b = TenantNamespace::new("org-b").unwrap();
        let result = reg.check_boundary(&ns_a, &ns_b, NamespacedResourceKind::Pane);
        assert!(!result.is_allowed());
        assert!(result.crosses_boundary);
        assert_eq!(result.decision, CrossTenantDecision::Deny);
    }

    #[test]
    fn system_namespace_bypasses_by_default() {
        let reg = NamespaceRegistry::new();
        let system = TenantNamespace::system();
        let target = TenantNamespace::new("org").unwrap();
        let result = reg.check_boundary(&system, &target, NamespacedResourceKind::Pane);
        assert!(result.is_allowed());
        assert!(result.crosses_boundary);
        assert_eq!(result.decision, CrossTenantDecision::AllowWithAudit);
        assert_eq!(result.matched_rule.as_deref(), Some("system_bypass"));
    }

    #[test]
    fn system_bypass_disabled() {
        let policy = CrossTenantPolicy {
            system_bypass: false,
            ..CrossTenantPolicy::default()
        };
        let reg = NamespaceRegistry::with_policy(policy);
        let system = TenantNamespace::system();
        let target = TenantNamespace::new("org").unwrap();
        let result = reg.check_boundary(&system, &target, NamespacedResourceKind::Pane);
        assert!(!result.is_allowed());
        assert_eq!(result.decision, CrossTenantDecision::Deny);
    }

    #[test]
    fn hierarchical_delegation() {
        let policy = CrossTenantPolicy {
            allow_hierarchical: true,
            ..CrossTenantPolicy::default()
        };
        let reg = NamespaceRegistry::with_policy(policy);
        let parent = TenantNamespace::new("org").unwrap();
        let child = TenantNamespace::new("org.team").unwrap();

        // Parent can access child.
        let result = reg.check_boundary(&parent, &child, NamespacedResourceKind::Pane);
        assert!(result.is_allowed());
        assert!(result.hierarchical_match);

        // Child cannot access parent (hierarchy is one-way downward).
        let result = reg.check_boundary(&child, &parent, NamespacedResourceKind::Pane);
        assert!(!result.is_allowed());
    }

    #[test]
    fn explicit_rule_overrides_default() {
        let ns_a = TenantNamespace::new("team-a").unwrap();
        let ns_b = TenantNamespace::new("team-b").unwrap();
        let policy = CrossTenantPolicy::default().with_rule(
            CrossTenantRule::allow_with_audit(ns_a.clone(), ns_b.clone())
                .with_reason("shared project"),
        );
        let reg = NamespaceRegistry::with_policy(policy);

        let result = reg.check_boundary(&ns_a, &ns_b, NamespacedResourceKind::Workflow);
        assert!(result.is_allowed());
        assert!(result.requires_audit());
        assert!(
            result
                .matched_rule
                .as_ref()
                .unwrap()
                .contains("shared project")
        );
    }

    #[test]
    fn rule_scoped_to_resource_kind() {
        let ns_a = TenantNamespace::new("a").unwrap();
        let ns_b = TenantNamespace::new("b").unwrap();
        let policy = CrossTenantPolicy::default().with_rule(
            CrossTenantRule::allow_with_audit(ns_a.clone(), ns_b.clone())
                .for_kinds([NamespacedResourceKind::Pane]),
        );
        let reg = NamespaceRegistry::with_policy(policy);

        // Rule applies to Pane.
        let result = reg.check_boundary(&ns_a, &ns_b, NamespacedResourceKind::Pane);
        assert!(result.is_allowed());

        // Rule does NOT apply to Workflow — falls to default deny.
        let result = reg.check_boundary(&ns_a, &ns_b, NamespacedResourceKind::Workflow);
        assert!(!result.is_allowed());
    }

    #[test]
    fn permissive_policy_allows_all() {
        let reg = NamespaceRegistry::with_policy(CrossTenantPolicy::permissive());
        let ns_a = TenantNamespace::new("a").unwrap();
        let ns_b = TenantNamespace::new("b").unwrap();
        let result = reg.check_boundary(&ns_a, &ns_b, NamespacedResourceKind::Connector);
        assert!(result.is_allowed());
        assert!(result.requires_audit());
    }

    // ---- Audit tests ----

    #[test]
    fn check_and_audit_records_entry() {
        let mut reg = NamespaceRegistry::with_policy(CrossTenantPolicy::permissive());
        let ns_a = TenantNamespace::new("a").unwrap();
        let ns_b = TenantNamespace::new("b").unwrap();

        let result = reg.check_and_audit(
            &ns_a,
            &ns_b,
            NamespacedResourceKind::Pane,
            "42",
            1_710_000_000_000,
        );
        assert!(result.is_allowed());
        assert_eq!(reg.audit_log().len(), 1);
        assert_eq!(reg.audit_log()[0].resource_id, "42");
    }

    #[test]
    fn same_namespace_no_audit() {
        let mut reg = NamespaceRegistry::with_policy(CrossTenantPolicy::permissive());
        let ns = TenantNamespace::new("org").unwrap();

        reg.check_and_audit(&ns, &ns, NamespacedResourceKind::Pane, "1", 1_000);
        assert!(reg.audit_log().is_empty());
    }

    #[test]
    fn audit_log_eviction() {
        let mut reg = NamespaceRegistry::with_policy(CrossTenantPolicy::permissive());
        reg.max_audit_entries = 3;
        let ns_a = TenantNamespace::new("a").unwrap();
        let ns_b = TenantNamespace::new("b").unwrap();

        for i in 0..5 {
            reg.check_and_audit(
                &ns_a,
                &ns_b,
                NamespacedResourceKind::Pane,
                &format!("p{i}"),
                i as u64,
            );
        }
        assert_eq!(reg.audit_log().len(), 3);
        // Oldest entries should be evicted.
        assert_eq!(reg.audit_log()[0].resource_id, "p2");
    }

    // ---- Telemetry tests ----

    #[test]
    fn telemetry_records_boundary_checks() {
        let mut telem = NamespaceIsolationTelemetry::default();

        // Same-namespace check.
        let same_ns = BoundaryCheckResult {
            crosses_boundary: false,
            source_namespace: TenantNamespace::default(),
            target_namespace: TenantNamespace::default(),
            decision: CrossTenantDecision::Allow,
            matched_rule: None,
            hierarchical_match: false,
        };
        telem.record(&same_ns);
        assert_eq!(telem.checks_total, 1);
        assert_eq!(telem.cross_boundary_total, 0);

        // Cross-boundary denied.
        let denied = BoundaryCheckResult {
            crosses_boundary: true,
            source_namespace: TenantNamespace::new("a").unwrap(),
            target_namespace: TenantNamespace::new("b").unwrap(),
            decision: CrossTenantDecision::Deny,
            matched_rule: None,
            hierarchical_match: false,
        };
        telem.record(&denied);
        assert_eq!(telem.checks_total, 2);
        assert_eq!(telem.cross_boundary_total, 1);
        assert_eq!(telem.cross_boundary_denied, 1);

        // System bypass.
        let bypass = BoundaryCheckResult {
            crosses_boundary: true,
            source_namespace: TenantNamespace::system(),
            target_namespace: TenantNamespace::new("x").unwrap(),
            decision: CrossTenantDecision::AllowWithAudit,
            matched_rule: Some("system_bypass".to_owned()),
            hierarchical_match: false,
        };
        telem.record(&bypass);
        assert_eq!(telem.system_bypass_grants, 1);
        assert_eq!(telem.cross_boundary_audited, 1);

        let snap = telem.snapshot();
        assert_eq!(snap.checks_total, 3);
        assert_eq!(snap.cross_boundary_total, 2);
    }

    // ---- Snapshot tests ----

    #[test]
    fn registry_snapshot() {
        let mut reg = NamespaceRegistry::new();
        let ns_a = TenantNamespace::new("a").unwrap();
        let ns_b = TenantNamespace::new("b").unwrap();

        reg.bind(NamespacedResourceKind::Pane, "1", ns_a.clone());
        reg.bind(NamespacedResourceKind::Pane, "2", ns_a.clone());
        reg.bind(NamespacedResourceKind::Agent, "bot1", ns_b.clone());

        let snap = reg.snapshot();
        assert_eq!(snap.total_bindings, 3);
        assert_eq!(snap.active_namespaces, 2);
        assert_eq!(snap.namespace_counts.get("a"), Some(&2));
        assert_eq!(snap.namespace_counts.get("b"), Some(&1));
    }

    // ---- NamespaceBinding tests ----

    #[test]
    fn binding_stable_key() {
        let binding = NamespaceBinding::new(
            TenantNamespace::new("org").unwrap(),
            NamespacedResourceKind::Pane,
            "42",
        );
        assert_eq!(binding.stable_key(), "pane:org:42");
    }

    // ---- CrossTenantRule tests ----

    #[test]
    fn rule_applies_to_all_when_empty() {
        let rule = CrossTenantRule::deny(
            TenantNamespace::new("a").unwrap(),
            TenantNamespace::new("b").unwrap(),
        );
        assert!(rule.applies_to(NamespacedResourceKind::Pane));
        assert!(rule.applies_to(NamespacedResourceKind::Credential));
    }

    #[test]
    fn rule_applies_to_specific_kinds() {
        let rule = CrossTenantRule::deny(
            TenantNamespace::new("a").unwrap(),
            TenantNamespace::new("b").unwrap(),
        )
        .for_kinds([NamespacedResourceKind::Credential]);

        assert!(rule.applies_to(NamespacedResourceKind::Credential));
        assert!(!rule.applies_to(NamespacedResourceKind::Pane));
    }

    // ---- Display tests ----

    #[test]
    fn namespace_display() {
        let ns = TenantNamespace::new("org.team").unwrap();
        assert_eq!(format!("{ns}"), "org.team");
    }
}
