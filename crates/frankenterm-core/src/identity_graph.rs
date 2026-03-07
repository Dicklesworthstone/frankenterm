//! Identity graph and least-privilege authorization model (ft-3681t.6.2).
//!
//! Maps agent identities, panes/sessions, workflows, connectors, and hosts
//! into a unified identity graph with least-privilege defaults and explicit
//! delegation rules. Enables reachability queries, transitive permission
//! checks, and conflict detection across the entire FrankenTerm authorization
//! surface.
//!
//! # Architecture
//!
//! The identity graph is a directed acyclic graph (DAG) with two node kinds:
//!
//! 1. **Principal nodes** — identities that can act (agents, humans, workflows,
//!    connectors, system components).
//! 2. **Resource nodes** — targets of actions (panes, sessions, windows,
//!    credentials, capabilities).
//!
//! Edges encode authorization relationships:
//! - **Grant** — principal P is authorized for action A on resource R.
//! - **Delegate** — principal P1 delegates a subset of its grants to P2.
//! - **MemberOf** — principal P belongs to group G (inherits G's grants).
//!
//! # Least-privilege defaults
//!
//! - New principals start with zero grants.
//! - Grants are scoped: (principal, action, resource, conditions).
//! - Delegation must be a strict subset of the delegator's grants.
//! - All authorization decisions are logged for audit.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

// =============================================================================
// Principal identity
// =============================================================================

/// Unified principal identity across all FrankenTerm subsystems.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PrincipalId {
    /// Kind of principal.
    pub kind: PrincipalKind,
    /// Unique identifier within that kind (e.g., agent name, connector ID).
    pub id: String,
    /// Optional namespace/domain qualifier.
    pub domain: Option<String>,
}

impl PrincipalId {
    #[must_use]
    pub fn new(kind: PrincipalKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
            domain: None,
        }
    }

    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Stable string key for indexing.
    #[must_use]
    pub fn stable_key(&self) -> String {
        match &self.domain {
            Some(d) => format!("{}:{}:{}", self.kind.as_str(), d, self.id),
            None => format!("{}:{}", self.kind.as_str(), self.id),
        }
    }

    /// Convenience constructors.
    #[must_use]
    pub fn agent(id: impl Into<String>) -> Self {
        Self::new(PrincipalKind::Agent, id)
    }

    #[must_use]
    pub fn human(id: impl Into<String>) -> Self {
        Self::new(PrincipalKind::Human, id)
    }

    #[must_use]
    pub fn connector(id: impl Into<String>) -> Self {
        Self::new(PrincipalKind::Connector, id)
    }

    #[must_use]
    pub fn workflow(id: impl Into<String>) -> Self {
        Self::new(PrincipalKind::Workflow, id)
    }

    #[must_use]
    pub fn system(id: impl Into<String>) -> Self {
        Self::new(PrincipalKind::System, id)
    }

    #[must_use]
    pub fn group(id: impl Into<String>) -> Self {
        Self::new(PrincipalKind::Group, id)
    }
}

impl std::fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stable_key())
    }
}

/// Kind of principal in the identity graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// Human operator.
    Human,
    /// AI agent (Claude, Codex, Gemini, etc.).
    Agent,
    /// External connector (GitHub, Slack, etc.).
    Connector,
    /// Automated workflow/pipeline.
    Workflow,
    /// System component (policy engine, credential broker, etc.).
    System,
    /// Group of principals (for role-based access).
    Group,
    /// MCP tool server.
    Mcp,
}

impl PrincipalKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Agent => "agent",
            Self::Connector => "connector",
            Self::Workflow => "workflow",
            Self::System => "system",
            Self::Group => "group",
            Self::Mcp => "mcp",
        }
    }

    /// Default trust level for this principal kind.
    #[must_use]
    pub const fn default_trust(self) -> TrustLevel {
        match self {
            Self::Human => TrustLevel::High,
            Self::System => TrustLevel::High,
            Self::Agent => TrustLevel::Standard,
            Self::Workflow => TrustLevel::Standard,
            Self::Mcp => TrustLevel::Low,
            Self::Connector => TrustLevel::Low,
            Self::Group => TrustLevel::Standard,
        }
    }
}

impl std::fmt::Display for PrincipalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Resource identity
// =============================================================================

/// A resource that can be the target of authorization.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ResourceId {
    /// Kind of resource.
    pub kind: ResourceKind,
    /// Unique identifier.
    pub id: String,
}

impl ResourceId {
    #[must_use]
    pub fn new(kind: ResourceKind, id: impl Into<String>) -> Self {
        Self {
            kind,
            id: id.into(),
        }
    }

    #[must_use]
    pub fn stable_key(&self) -> String {
        format!("{}:{}", self.kind.as_str(), self.id)
    }

    // Convenience constructors
    #[must_use]
    pub fn pane(id: impl Into<String>) -> Self {
        Self::new(ResourceKind::Pane, id)
    }

    #[must_use]
    pub fn session(id: impl Into<String>) -> Self {
        Self::new(ResourceKind::Session, id)
    }

    #[must_use]
    pub fn window(id: impl Into<String>) -> Self {
        Self::new(ResourceKind::Window, id)
    }

    #[must_use]
    pub fn credential(id: impl Into<String>) -> Self {
        Self::new(ResourceKind::Credential, id)
    }

    #[must_use]
    pub fn fleet() -> Self {
        Self::new(ResourceKind::Fleet, "*")
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.kind.as_str(), self.id)
    }
}

/// Kind of resource in the identity graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Pane,
    Window,
    Session,
    Credential,
    Capability,
    Workflow,
    Fleet,
    File,
    Network,
}

impl ResourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Window => "window",
            Self::Session => "session",
            Self::Credential => "credential",
            Self::Capability => "capability",
            Self::Workflow => "workflow",
            Self::Fleet => "fleet",
            Self::File => "file",
            Self::Network => "network",
        }
    }
}

impl std::fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Trust level
// =============================================================================

/// Trust level assigned to a principal (ordered from least to most trusted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Untrusted — default for unknown/unverified principals.
    Untrusted,
    /// Low trust — connectors, MCP servers.
    Low,
    /// Standard trust — authenticated agents, workflows.
    Standard,
    /// High trust — humans, system components.
    High,
    /// Admin — full access, bypasses most checks.
    Admin,
}

impl TrustLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Low => "low",
            Self::Standard => "standard",
            Self::High => "high",
            Self::Admin => "admin",
        }
    }

    /// Whether this trust level satisfies a minimum requirement.
    #[must_use]
    pub fn satisfies(self, minimum: Self) -> bool {
        self >= minimum
    }
}

impl std::fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Actions
// =============================================================================

/// Actions that can be authorized in the identity graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthAction {
    /// Read from a resource.
    Read,
    /// Write/mutate a resource.
    Write,
    /// Execute/invoke an operation.
    Execute,
    /// Create a new resource.
    Create,
    /// Delete/destroy a resource.
    Delete,
    /// Administer (change permissions, configuration).
    Admin,
    /// Delegate permissions to another principal.
    Delegate,
    /// Custom action for extensibility.
    Custom(String),
}

impl AuthAction {
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Admin => "admin",
            Self::Delegate => "delegate",
            Self::Custom(s) => s.as_str(),
        }
    }

    /// Whether this action is considered destructive.
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::Delete | Self::Admin)
    }

    /// Whether this action is mutating.
    #[must_use]
    pub fn is_mutating(&self) -> bool {
        !matches!(self, Self::Read)
    }

    /// Minimum trust level typically required for this action.
    #[must_use]
    pub fn default_min_trust(&self) -> TrustLevel {
        match self {
            Self::Read => TrustLevel::Low,
            Self::Write | Self::Execute | Self::Create => TrustLevel::Standard,
            Self::Delete | Self::Admin | Self::Delegate => TrustLevel::High,
            Self::Custom(_) => TrustLevel::Standard,
        }
    }
}

impl std::fmt::Display for AuthAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// =============================================================================
// Authorization grant
// =============================================================================

/// A grant authorizing a principal for specific actions on a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthGrant {
    /// Unique grant identifier.
    pub grant_id: String,
    /// Principal receiving the grant.
    pub principal: PrincipalId,
    /// Authorized actions.
    pub actions: BTreeSet<AuthAction>,
    /// Target resource (or pattern).
    pub resource: ResourceId,
    /// Conditions that must be met for the grant to apply.
    pub conditions: Vec<GrantCondition>,
    /// Whether this grant is currently active.
    pub active: bool,
    /// When the grant was created (epoch ms).
    pub created_at_ms: u64,
    /// When the grant expires (None = no expiry).
    pub expires_at_ms: Option<u64>,
    /// Who created this grant.
    pub granted_by: Option<PrincipalId>,
    /// Optional human-readable reason.
    pub reason: Option<String>,
}

impl AuthGrant {
    /// Create a new active grant with no conditions.
    #[must_use]
    pub fn new(
        grant_id: impl Into<String>,
        principal: PrincipalId,
        actions: BTreeSet<AuthAction>,
        resource: ResourceId,
    ) -> Self {
        let now_ms = now_epoch_ms();
        Self {
            grant_id: grant_id.into(),
            principal,
            actions,
            resource,
            conditions: Vec::new(),
            active: true,
            created_at_ms: now_ms,
            expires_at_ms: None,
            granted_by: None,
            reason: None,
        }
    }

    /// Whether this grant is currently valid.
    #[must_use]
    pub fn is_valid(&self, now_ms: u64) -> bool {
        if !self.active {
            return false;
        }
        if let Some(exp) = self.expires_at_ms {
            if now_ms > exp {
                return false;
            }
        }
        true
    }

    /// Whether this grant covers a specific action on a specific resource.
    #[must_use]
    pub fn covers(&self, action: &AuthAction, resource: &ResourceId, now_ms: u64) -> bool {
        if !self.is_valid(now_ms) {
            return false;
        }
        if !self.actions.contains(action) {
            return false;
        }
        self.resource_matches(resource)
    }

    /// Check if this grant's resource matches the target.
    #[must_use]
    fn resource_matches(&self, target: &ResourceId) -> bool {
        // Fleet resource matches all resources of any kind
        if self.resource.kind == ResourceKind::Fleet && self.resource.id == "*" {
            return true;
        }
        // Same kind, exact ID or wildcard
        if self.resource.kind != target.kind {
            return false;
        }
        self.resource.id == target.id || self.resource.id == "*"
    }

    /// Whether this grant is a subset of another (for delegation validation).
    #[must_use]
    pub fn is_subset_of(&self, parent: &AuthGrant) -> bool {
        // Actions must be a subset
        if !self.actions.is_subset(&parent.actions) {
            return false;
        }
        // Resource must match parent's resource
        parent.resource_matches(&self.resource)
    }
}

/// Conditions attached to a grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantCondition {
    /// Only valid during specific time windows.
    TimeWindow { start_ms: u64, end_ms: u64 },
    /// Only valid when principal's trust level meets minimum.
    MinTrust(TrustLevel),
    /// Only valid for a specific domain/namespace.
    Domain(String),
    /// Requires approval from the specified principal before each use.
    RequiresApproval(PrincipalId),
    /// Rate-limited to N uses per window_ms.
    RateLimit { max_uses: u32, window_ms: u64 },
}

// =============================================================================
// Delegation edge
// =============================================================================

/// A delegation from one principal to another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delegation {
    /// Unique delegation identifier.
    pub delegation_id: String,
    /// Principal delegating authority.
    pub delegator: PrincipalId,
    /// Principal receiving delegated authority.
    pub delegate: PrincipalId,
    /// Scope of delegation (which grants are delegated).
    pub scope: DelegationScope,
    /// Whether this delegation is active.
    pub active: bool,
    /// When the delegation was created.
    pub created_at_ms: u64,
    /// When the delegation expires.
    pub expires_at_ms: Option<u64>,
}

impl Delegation {
    #[must_use]
    pub fn is_valid(&self, now_ms: u64) -> bool {
        if !self.active {
            return false;
        }
        if let Some(exp) = self.expires_at_ms {
            if now_ms > exp {
                return false;
            }
        }
        true
    }
}

/// What is being delegated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationScope {
    /// Specific grant IDs.
    Grants(Vec<String>),
    /// All grants on specific resources.
    Resources(Vec<ResourceId>),
    /// All grants for specific actions.
    Actions(BTreeSet<AuthAction>),
    /// All non-admin grants (safe subset).
    AllNonAdmin,
}

// =============================================================================
// Group membership
// =============================================================================

/// Membership of a principal in a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMembership {
    /// The group.
    pub group: PrincipalId,
    /// The member.
    pub member: PrincipalId,
    /// When membership was added.
    pub added_at_ms: u64,
    /// When membership expires (None = no expiry).
    pub expires_at_ms: Option<u64>,
    /// Whether membership is active.
    pub active: bool,
}

impl GroupMembership {
    #[must_use]
    pub fn is_valid(&self, now_ms: u64) -> bool {
        if !self.active {
            return false;
        }
        if let Some(exp) = self.expires_at_ms {
            if now_ms > exp {
                return false;
            }
        }
        true
    }
}

// =============================================================================
// Authorization decision
// =============================================================================

/// Result of an authorization query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthzDecision {
    /// Authorized — includes which grant(s) provided the authorization.
    Allow { grant_ids: Vec<String> },
    /// Denied — includes reason.
    Deny { reason: String },
    /// Requires approval from specific principal.
    RequireApproval {
        approver: PrincipalId,
        reason: String,
    },
}

impl AuthzDecision {
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }
}

impl std::fmt::Display for AuthzDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allow { grant_ids } => write!(f, "allow (grants: {})", grant_ids.join(", ")),
            Self::Deny { reason } => write!(f, "deny: {reason}"),
            Self::RequireApproval { approver, reason } => {
                write!(f, "require_approval({approver}): {reason}")
            }
        }
    }
}

// =============================================================================
// Authorization audit entry
// =============================================================================

/// Audit record for an authorization decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzAuditEntry {
    /// Principal requesting authorization.
    pub principal: PrincipalId,
    /// Action requested.
    pub action: AuthAction,
    /// Target resource.
    pub resource: ResourceId,
    /// Decision made.
    pub decision: AuthzDecision,
    /// Whether authorization was via delegation chain.
    pub via_delegation: bool,
    /// Whether authorization was via group membership.
    pub via_group: bool,
    /// Timestamp.
    pub timestamp_ms: u64,
}

// =============================================================================
// Identity graph telemetry
// =============================================================================

/// Telemetry for the identity graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityGraphTelemetry {
    pub principals_registered: u64,
    pub grants_active: u64,
    pub grants_expired: u64,
    pub grants_revoked: u64,
    pub delegations_active: u64,
    pub group_memberships: u64,
    pub authz_queries: u64,
    pub authz_allowed: u64,
    pub authz_denied: u64,
    pub authz_approval_required: u64,
    pub delegation_violations: u64,
}

// =============================================================================
// Identity graph errors
// =============================================================================

/// Errors from identity graph operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityGraphError {
    /// Principal not found.
    PrincipalNotFound { id: String },
    /// Grant not found.
    GrantNotFound { grant_id: String },
    /// Delegation would exceed delegator's authority.
    DelegationExceedsAuthority { reason: String },
    /// Circular delegation detected.
    CircularDelegation { chain: Vec<String> },
    /// Group is not a Group-kind principal.
    NotAGroup { id: String },
    /// Duplicate grant ID.
    DuplicateGrant { grant_id: String },
    /// Duplicate principal ID.
    DuplicatePrincipal { id: String },
}

impl std::fmt::Display for IdentityGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrincipalNotFound { id } => write!(f, "principal not found: {id}"),
            Self::GrantNotFound { grant_id } => write!(f, "grant not found: {grant_id}"),
            Self::DelegationExceedsAuthority { reason } => {
                write!(f, "delegation exceeds authority: {reason}")
            }
            Self::CircularDelegation { chain } => {
                write!(f, "circular delegation: {}", chain.join(" -> "))
            }
            Self::NotAGroup { id } => write!(f, "not a group: {id}"),
            Self::DuplicateGrant { grant_id } => write!(f, "duplicate grant: {grant_id}"),
            Self::DuplicatePrincipal { id } => write!(f, "duplicate principal: {id}"),
        }
    }
}

// =============================================================================
// Identity graph engine
// =============================================================================

/// The identity graph: a DAG of principals, resources, grants, delegations,
/// and group memberships with least-privilege authorization queries.
pub struct IdentityGraph {
    /// Registered principals with their trust levels.
    principals: HashMap<String, PrincipalRecord>,
    /// All grants indexed by grant_id.
    grants: HashMap<String, AuthGrant>,
    /// Grants indexed by principal stable_key.
    grants_by_principal: HashMap<String, Vec<String>>,
    /// Delegations indexed by delegation_id.
    delegations: HashMap<String, Delegation>,
    /// Delegations indexed by delegate's stable_key.
    delegations_by_delegate: HashMap<String, Vec<String>>,
    /// Group memberships indexed by member's stable_key.
    memberships_by_member: HashMap<String, Vec<GroupMembership>>,
    /// Audit log (bounded).
    audit_log: VecDeque<AuthzAuditEntry>,
    /// Max audit entries.
    max_audit_entries: usize,
    /// Telemetry.
    telemetry: IdentityGraphTelemetry,
}

/// Internal record for a registered principal.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrincipalRecord {
    id: PrincipalId,
    trust_level: TrustLevel,
    registered_at_ms: u64,
    active: bool,
}

impl IdentityGraph {
    /// Create a new empty identity graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            principals: HashMap::new(),
            grants: HashMap::new(),
            grants_by_principal: HashMap::new(),
            delegations: HashMap::new(),
            delegations_by_delegate: HashMap::new(),
            memberships_by_member: HashMap::new(),
            audit_log: VecDeque::new(),
            max_audit_entries: 10_000,
            telemetry: IdentityGraphTelemetry::default(),
        }
    }

    /// Create with custom audit log limit.
    #[must_use]
    pub fn with_audit_limit(mut self, limit: usize) -> Self {
        self.max_audit_entries = limit;
        self
    }

    // ── Principal management ──

    /// Register a new principal with default trust for its kind.
    pub fn register_principal(&mut self, id: PrincipalId) -> Result<(), IdentityGraphError> {
        let key = id.stable_key();
        if self.principals.contains_key(&key) {
            return Err(IdentityGraphError::DuplicatePrincipal { id: key });
        }
        let trust = id.kind.default_trust();
        self.principals.insert(
            key,
            PrincipalRecord {
                id,
                trust_level: trust,
                registered_at_ms: now_epoch_ms(),
                active: true,
            },
        );
        self.telemetry.principals_registered += 1;
        Ok(())
    }

    /// Register a principal with explicit trust level.
    pub fn register_principal_with_trust(
        &mut self,
        id: PrincipalId,
        trust: TrustLevel,
    ) -> Result<(), IdentityGraphError> {
        let key = id.stable_key();
        if self.principals.contains_key(&key) {
            return Err(IdentityGraphError::DuplicatePrincipal { id: key });
        }
        self.principals.insert(
            key,
            PrincipalRecord {
                id,
                trust_level: trust,
                registered_at_ms: now_epoch_ms(),
                active: true,
            },
        );
        self.telemetry.principals_registered += 1;
        Ok(())
    }

    /// Update a principal's trust level.
    pub fn set_trust(
        &mut self,
        principal: &PrincipalId,
        trust: TrustLevel,
    ) -> Result<(), IdentityGraphError> {
        let key = principal.stable_key();
        let record = self
            .principals
            .get_mut(&key)
            .ok_or(IdentityGraphError::PrincipalNotFound { id: key })?;
        record.trust_level = trust;
        Ok(())
    }

    /// Get a principal's trust level.
    #[must_use]
    pub fn trust_level(&self, principal: &PrincipalId) -> Option<TrustLevel> {
        self.principals
            .get(&principal.stable_key())
            .map(|r| r.trust_level)
    }

    /// Check if a principal is registered and active.
    #[must_use]
    pub fn is_registered(&self, principal: &PrincipalId) -> bool {
        self.principals
            .get(&principal.stable_key())
            .is_some_and(|r| r.active)
    }

    /// Deactivate a principal (revokes all grants).
    pub fn deactivate_principal(
        &mut self,
        principal: &PrincipalId,
    ) -> Result<(), IdentityGraphError> {
        let key = principal.stable_key();
        let record = self
            .principals
            .get_mut(&key)
            .ok_or_else(|| IdentityGraphError::PrincipalNotFound { id: key.clone() })?;
        record.active = false;

        // Revoke all grants
        if let Some(grant_ids) = self.grants_by_principal.get(&key) {
            for gid in grant_ids.clone() {
                if let Some(grant) = self.grants.get_mut(&gid) {
                    if grant.active {
                        grant.active = false;
                        self.telemetry.grants_revoked += 1;
                    }
                }
            }
        }

        Ok(())
    }

    /// Count of registered principals.
    #[must_use]
    pub fn principal_count(&self) -> usize {
        self.principals.len()
    }

    // ── Grant management ──

    /// Add an authorization grant.
    pub fn add_grant(&mut self, grant: AuthGrant) -> Result<(), IdentityGraphError> {
        let grant_id = grant.grant_id.clone();
        if self.grants.contains_key(&grant_id) {
            return Err(IdentityGraphError::DuplicateGrant { grant_id });
        }
        let principal_key = grant.principal.stable_key();
        self.grants_by_principal
            .entry(principal_key)
            .or_default()
            .push(grant_id.clone());
        self.grants.insert(grant_id, grant);
        self.telemetry.grants_active += 1;
        Ok(())
    }

    /// Revoke a grant by ID.
    pub fn revoke_grant(&mut self, grant_id: &str) -> Result<(), IdentityGraphError> {
        let grant =
            self.grants
                .get_mut(grant_id)
                .ok_or_else(|| IdentityGraphError::GrantNotFound {
                    grant_id: grant_id.to_string(),
                })?;
        if grant.active {
            grant.active = false;
            self.telemetry.grants_revoked += 1;
        }
        Ok(())
    }

    /// List active grants for a principal.
    #[must_use]
    pub fn grants_for(&self, principal: &PrincipalId, now_ms: u64) -> Vec<&AuthGrant> {
        let key = principal.stable_key();
        self.grants_by_principal
            .get(&key)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.grants.get(id))
                    .filter(|g| g.is_valid(now_ms))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Count of active grants.
    #[must_use]
    pub fn active_grant_count(&self) -> usize {
        self.grants.values().filter(|g| g.active).count()
    }

    // ── Delegation management ──

    /// Add a delegation from one principal to another.
    pub fn add_delegation(&mut self, delegation: Delegation) -> Result<(), IdentityGraphError> {
        // Check for circular delegation
        let delegator_key = delegation.delegator.stable_key();
        let delegate_key = delegation.delegate.stable_key();

        if delegator_key == delegate_key {
            return Err(IdentityGraphError::CircularDelegation {
                chain: vec![delegator_key],
            });
        }

        // Check for cycles in delegation chain
        if self.has_delegation_path(&delegation.delegate, &delegation.delegator) {
            return Err(IdentityGraphError::CircularDelegation {
                chain: vec![delegator_key, delegate_key],
            });
        }

        self.delegations_by_delegate
            .entry(delegate_key)
            .or_default()
            .push(delegation.delegation_id.clone());

        self.delegations
            .insert(delegation.delegation_id.clone(), delegation);
        self.telemetry.delegations_active += 1;
        Ok(())
    }

    /// Check if there's a delegation path from `from` to `to`.
    fn has_delegation_path(&self, from: &PrincipalId, to: &PrincipalId) -> bool {
        let target_key = to.stable_key();
        let mut visited = BTreeSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(from.stable_key());

        while let Some(current) = queue.pop_front() {
            if current == target_key {
                return true;
            }
            if !visited.insert(current.clone()) {
                continue;
            }
            // Find delegations where current is the delegate
            if let Some(del_ids) = self.delegations_by_delegate.get(&current) {
                for did in del_ids {
                    if let Some(d) = self.delegations.get(did) {
                        if d.active {
                            queue.push_back(d.delegator.stable_key());
                        }
                    }
                }
            }
        }
        false
    }

    // ── Group membership management ──

    /// Add a principal to a group.
    pub fn add_to_group(
        &mut self,
        group: &PrincipalId,
        member: &PrincipalId,
    ) -> Result<(), IdentityGraphError> {
        if group.kind != PrincipalKind::Group {
            return Err(IdentityGraphError::NotAGroup {
                id: group.stable_key(),
            });
        }
        if !self.is_registered(group) {
            return Err(IdentityGraphError::PrincipalNotFound {
                id: group.stable_key(),
            });
        }

        let membership = GroupMembership {
            group: group.clone(),
            member: member.clone(),
            added_at_ms: now_epoch_ms(),
            expires_at_ms: None,
            active: true,
        };

        self.memberships_by_member
            .entry(member.stable_key())
            .or_default()
            .push(membership);
        self.telemetry.group_memberships += 1;
        Ok(())
    }

    /// Get all active groups a principal belongs to.
    #[must_use]
    pub fn groups_of(&self, member: &PrincipalId, now_ms: u64) -> Vec<&PrincipalId> {
        self.memberships_by_member
            .get(&member.stable_key())
            .map(|memberships| {
                memberships
                    .iter()
                    .filter(|m| m.is_valid(now_ms))
                    .map(|m| &m.group)
                    .collect()
            })
            .unwrap_or_default()
    }

    // ── Authorization queries ──

    /// Check if a principal is authorized for an action on a resource.
    /// This is the main authorization entry point.
    pub fn authorize(
        &mut self,
        principal: &PrincipalId,
        action: &AuthAction,
        resource: &ResourceId,
    ) -> AuthzDecision {
        let now_ms = now_epoch_ms();
        self.telemetry.authz_queries += 1;

        // Check if principal is registered and active
        if !self.is_registered(principal) {
            let decision = AuthzDecision::Deny {
                reason: format!("principal not registered: {}", principal.stable_key()),
            };
            self.record_audit(principal, action, resource, &decision, false, false);
            self.telemetry.authz_denied += 1;
            return decision;
        }

        // Check trust level against action's minimum
        let trust = self.trust_level(principal).unwrap_or(TrustLevel::Untrusted);
        let min_trust = action.default_min_trust();
        if !trust.satisfies(min_trust) {
            let decision = AuthzDecision::Deny {
                reason: format!("insufficient trust: have {trust}, need {min_trust} for {action}"),
            };
            self.record_audit(principal, action, resource, &decision, false, false);
            self.telemetry.authz_denied += 1;
            return decision;
        }

        // Check direct grants
        let direct_grants = self.find_covering_grants(principal, action, resource, now_ms);
        if !direct_grants.is_empty() {
            let decision = AuthzDecision::Allow {
                grant_ids: direct_grants,
            };
            self.record_audit(principal, action, resource, &decision, false, false);
            self.telemetry.authz_allowed += 1;
            return decision;
        }

        // Check group-inherited grants
        let group_grants = self.find_group_grants(principal, action, resource, now_ms);
        if !group_grants.is_empty() {
            let decision = AuthzDecision::Allow {
                grant_ids: group_grants,
            };
            self.record_audit(principal, action, resource, &decision, false, true);
            self.telemetry.authz_allowed += 1;
            return decision;
        }

        // Check delegated grants
        let delegated_grants = self.find_delegated_grants(principal, action, resource, now_ms);
        if !delegated_grants.is_empty() {
            let decision = AuthzDecision::Allow {
                grant_ids: delegated_grants,
            };
            self.record_audit(principal, action, resource, &decision, true, false);
            self.telemetry.authz_allowed += 1;
            return decision;
        }

        // Check conditions that require approval
        let approval_grants = self.find_approval_grants(principal, action, resource, now_ms);
        if let Some((approver, grant_ids)) = approval_grants {
            let decision = AuthzDecision::RequireApproval {
                approver,
                reason: format!(
                    "grant(s) {} require approval for {action} on {resource}",
                    grant_ids.join(", ")
                ),
            };
            self.record_audit(principal, action, resource, &decision, false, false);
            self.telemetry.authz_approval_required += 1;
            return decision;
        }

        // Default deny
        let decision = AuthzDecision::Deny {
            reason: format!("no grant covers {action} on {resource}"),
        };
        self.record_audit(principal, action, resource, &decision, false, false);
        self.telemetry.authz_denied += 1;
        decision
    }

    /// Find grants that directly cover the requested action.
    fn find_covering_grants(
        &self,
        principal: &PrincipalId,
        action: &AuthAction,
        resource: &ResourceId,
        now_ms: u64,
    ) -> Vec<String> {
        self.grants_for(principal, now_ms)
            .into_iter()
            .filter(|g| g.covers(action, resource, now_ms))
            .filter(|g| {
                !g.conditions
                    .iter()
                    .any(|c| matches!(c, GrantCondition::RequiresApproval(_)))
            })
            .map(|g| g.grant_id.clone())
            .collect()
    }

    /// Find grants via group membership.
    fn find_group_grants(
        &self,
        principal: &PrincipalId,
        action: &AuthAction,
        resource: &ResourceId,
        now_ms: u64,
    ) -> Vec<String> {
        let groups = self.groups_of(principal, now_ms);
        let mut grant_ids = Vec::new();
        for group in groups {
            let group_grants = self.find_covering_grants(group, action, resource, now_ms);
            grant_ids.extend(group_grants);
        }
        grant_ids
    }

    /// Find grants via delegation chain.
    fn find_delegated_grants(
        &self,
        principal: &PrincipalId,
        action: &AuthAction,
        resource: &ResourceId,
        now_ms: u64,
    ) -> Vec<String> {
        let key = principal.stable_key();
        let del_ids = match self.delegations_by_delegate.get(&key) {
            Some(ids) => ids.clone(),
            None => return Vec::new(),
        };

        let mut grant_ids = Vec::new();
        for did in &del_ids {
            let delegation = match self.delegations.get(did) {
                Some(d) if d.is_valid(now_ms) => d,
                _ => continue,
            };

            // Check if delegator has the authority
            let delegator_grants =
                self.find_covering_grants(&delegation.delegator, action, resource, now_ms);
            if !delegator_grants.is_empty() {
                // Verify the delegation scope covers this action
                let scope_ok = match &delegation.scope {
                    DelegationScope::AllNonAdmin => !action.is_destructive(),
                    DelegationScope::Actions(actions) => actions.contains(action),
                    DelegationScope::Resources(resources) => {
                        resources.iter().any(|r| r == resource || r.id == "*")
                    }
                    DelegationScope::Grants(gids) => {
                        delegator_grants.iter().any(|g| gids.contains(g))
                    }
                };
                if scope_ok {
                    grant_ids.extend(delegator_grants);
                }
            }
        }
        grant_ids
    }

    /// Find grants that require approval.
    fn find_approval_grants(
        &self,
        principal: &PrincipalId,
        action: &AuthAction,
        resource: &ResourceId,
        now_ms: u64,
    ) -> Option<(PrincipalId, Vec<String>)> {
        let grants = self.grants_for(principal, now_ms);
        for grant in grants {
            if !grant.actions.contains(action) || !grant.resource_matches(resource) {
                continue;
            }
            for cond in &grant.conditions {
                if let GrantCondition::RequiresApproval(approver) = cond {
                    return Some((approver.clone(), vec![grant.grant_id.clone()]));
                }
            }
        }
        None
    }

    /// Record an audit entry.
    fn record_audit(
        &mut self,
        principal: &PrincipalId,
        action: &AuthAction,
        resource: &ResourceId,
        decision: &AuthzDecision,
        via_delegation: bool,
        via_group: bool,
    ) {
        let entry = AuthzAuditEntry {
            principal: principal.clone(),
            action: action.clone(),
            resource: resource.clone(),
            decision: decision.clone(),
            via_delegation,
            via_group,
            timestamp_ms: now_epoch_ms(),
        };
        if self.audit_log.len() >= self.max_audit_entries {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(entry);
    }

    // ── Query helpers ──

    /// List all resources a principal can access with a given action.
    #[must_use]
    pub fn accessible_resources(
        &self,
        principal: &PrincipalId,
        action: &AuthAction,
        now_ms: u64,
    ) -> Vec<&ResourceId> {
        self.grants_for(principal, now_ms)
            .into_iter()
            .filter(|g| g.actions.contains(action))
            .map(|g| &g.resource)
            .collect()
    }

    /// List all principals that have access to a resource.
    #[must_use]
    pub fn who_can_access(
        &self,
        resource: &ResourceId,
        action: &AuthAction,
        now_ms: u64,
    ) -> Vec<&PrincipalId> {
        let mut result = Vec::new();
        for record in self.principals.values() {
            if !record.active {
                continue;
            }
            let grants = self.find_covering_grants(&record.id, action, resource, now_ms);
            if !grants.is_empty() {
                result.push(&record.id);
            }
        }
        result
    }

    /// Expire all time-expired grants and delegations.
    pub fn expire_stale(&mut self, now_ms: u64) -> u32 {
        let mut expired = 0;
        for grant in self.grants.values_mut() {
            if grant.active {
                if let Some(exp) = grant.expires_at_ms {
                    if now_ms > exp {
                        grant.active = false;
                        expired += 1;
                        self.telemetry.grants_expired += 1;
                    }
                }
            }
        }
        for delegation in self.delegations.values_mut() {
            if delegation.active {
                if let Some(exp) = delegation.expires_at_ms {
                    if now_ms > exp {
                        delegation.active = false;
                        expired += 1;
                    }
                }
            }
        }
        expired
    }

    // ── Accessors ──

    #[must_use]
    pub fn audit_log(&self) -> &VecDeque<AuthzAuditEntry> {
        &self.audit_log
    }

    #[must_use]
    pub fn telemetry(&self) -> &IdentityGraphTelemetry {
        &self.telemetry
    }

    /// Serialize audit log to JSON.
    pub fn audit_log_json(&self) -> Result<String, serde_json::Error> {
        let entries: Vec<&AuthzAuditEntry> = self.audit_log.iter().collect();
        serde_json::to_string_pretty(&entries)
    }

    /// Summary of the graph contents.
    #[must_use]
    pub fn summary(&self) -> BTreeMap<String, usize> {
        let mut s = BTreeMap::new();
        s.insert("principals".into(), self.principals.len());
        s.insert("grants".into(), self.grants.len());
        s.insert(
            "active_grants".into(),
            self.grants.values().filter(|g| g.active).count(),
        );
        s.insert("delegations".into(), self.delegations.len());
        s.insert(
            "group_memberships".into(),
            self.memberships_by_member.values().map(|v| v.len()).sum(),
        );
        s.insert("audit_entries".into(), self.audit_log.len());
        s
    }
}

impl Default for IdentityGraph {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helpers ──

    fn test_graph() -> IdentityGraph {
        let mut g = IdentityGraph::new();
        g.register_principal(PrincipalId::human("operator-1"))
            .unwrap();
        g.register_principal(PrincipalId::agent("claude-1"))
            .unwrap();
        g.register_principal(PrincipalId::connector("github-1"))
            .unwrap();
        g.register_principal(PrincipalId::workflow("deploy-1"))
            .unwrap();
        g
    }

    fn read_write_actions() -> BTreeSet<AuthAction> {
        let mut s = BTreeSet::new();
        s.insert(AuthAction::Read);
        s.insert(AuthAction::Write);
        s
    }

    fn read_action() -> BTreeSet<AuthAction> {
        let mut s = BTreeSet::new();
        s.insert(AuthAction::Read);
        s
    }

    fn all_basic_actions() -> BTreeSet<AuthAction> {
        let mut s = BTreeSet::new();
        s.insert(AuthAction::Read);
        s.insert(AuthAction::Write);
        s.insert(AuthAction::Execute);
        s.insert(AuthAction::Create);
        s
    }

    // ── PrincipalId ──

    #[test]
    fn principal_stable_key() {
        let p = PrincipalId::agent("claude-1");
        assert_eq!(p.stable_key(), "agent:claude-1");

        let p = PrincipalId::agent("claude-1").with_domain("prod");
        assert_eq!(p.stable_key(), "agent:prod:claude-1");
    }

    #[test]
    fn principal_display() {
        let p = PrincipalId::human("alice");
        assert_eq!(p.to_string(), "human:alice");
    }

    #[test]
    fn principal_kind_display() {
        assert_eq!(PrincipalKind::Agent.to_string(), "agent");
        assert_eq!(PrincipalKind::Human.to_string(), "human");
    }

    #[test]
    fn principal_kind_serde_roundtrip() {
        for kind in [
            PrincipalKind::Human,
            PrincipalKind::Agent,
            PrincipalKind::Connector,
            PrincipalKind::Workflow,
            PrincipalKind::System,
            PrincipalKind::Group,
            PrincipalKind::Mcp,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let rt: PrincipalKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, rt);
        }
    }

    #[test]
    fn principal_default_trust() {
        assert_eq!(PrincipalKind::Human.default_trust(), TrustLevel::High);
        assert_eq!(PrincipalKind::Agent.default_trust(), TrustLevel::Standard);
        assert_eq!(PrincipalKind::Connector.default_trust(), TrustLevel::Low);
    }

    // ── ResourceId ──

    #[test]
    fn resource_stable_key() {
        let r = ResourceId::pane("42");
        assert_eq!(r.stable_key(), "pane:42");
    }

    #[test]
    fn resource_display() {
        assert_eq!(ResourceId::session("s1").to_string(), "session:s1");
    }

    #[test]
    fn resource_kind_serde_roundtrip() {
        for kind in [
            ResourceKind::Pane,
            ResourceKind::Window,
            ResourceKind::Session,
            ResourceKind::Credential,
            ResourceKind::Capability,
            ResourceKind::Workflow,
            ResourceKind::Fleet,
            ResourceKind::File,
            ResourceKind::Network,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            let rt: ResourceKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, rt);
        }
    }

    // ── TrustLevel ──

    #[test]
    fn trust_ordering() {
        assert!(TrustLevel::Untrusted < TrustLevel::Low);
        assert!(TrustLevel::Low < TrustLevel::Standard);
        assert!(TrustLevel::Standard < TrustLevel::High);
        assert!(TrustLevel::High < TrustLevel::Admin);
    }

    #[test]
    fn trust_satisfies() {
        assert!(TrustLevel::High.satisfies(TrustLevel::Standard));
        assert!(TrustLevel::Standard.satisfies(TrustLevel::Standard));
        assert!(!TrustLevel::Low.satisfies(TrustLevel::Standard));
    }

    #[test]
    fn trust_serde_roundtrip() {
        for t in [
            TrustLevel::Untrusted,
            TrustLevel::Low,
            TrustLevel::Standard,
            TrustLevel::High,
            TrustLevel::Admin,
        ] {
            let json = serde_json::to_string(&t).unwrap();
            let rt: TrustLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(t, rt);
        }
    }

    // ── AuthAction ──

    #[test]
    fn action_destructive() {
        assert!(AuthAction::Delete.is_destructive());
        assert!(AuthAction::Admin.is_destructive());
        assert!(!AuthAction::Read.is_destructive());
        assert!(!AuthAction::Write.is_destructive());
    }

    #[test]
    fn action_mutating() {
        assert!(!AuthAction::Read.is_mutating());
        assert!(AuthAction::Write.is_mutating());
        assert!(AuthAction::Delete.is_mutating());
    }

    #[test]
    fn action_default_min_trust() {
        assert_eq!(AuthAction::Read.default_min_trust(), TrustLevel::Low);
        assert_eq!(AuthAction::Write.default_min_trust(), TrustLevel::Standard);
        assert_eq!(AuthAction::Delete.default_min_trust(), TrustLevel::High);
    }

    #[test]
    fn action_serde_roundtrip() {
        let actions = vec![
            AuthAction::Read,
            AuthAction::Write,
            AuthAction::Execute,
            AuthAction::Create,
            AuthAction::Delete,
            AuthAction::Admin,
            AuthAction::Delegate,
            AuthAction::Custom("deploy".into()),
        ];
        for a in actions {
            let json = serde_json::to_string(&a).unwrap();
            let rt: AuthAction = serde_json::from_str(&json).unwrap();
            assert_eq!(a, rt);
        }
    }

    // ── AuthGrant ──

    #[test]
    fn grant_validity() {
        let grant = AuthGrant::new(
            "g1",
            PrincipalId::agent("a1"),
            read_action(),
            ResourceId::pane("p1"),
        );
        assert!(grant.is_valid(now_epoch_ms()));
    }

    #[test]
    fn grant_expired() {
        let mut grant = AuthGrant::new(
            "g1",
            PrincipalId::agent("a1"),
            read_action(),
            ResourceId::pane("p1"),
        );
        grant.expires_at_ms = Some(1000);
        assert!(!grant.is_valid(2000));
    }

    #[test]
    fn grant_inactive() {
        let mut grant = AuthGrant::new(
            "g1",
            PrincipalId::agent("a1"),
            read_action(),
            ResourceId::pane("p1"),
        );
        grant.active = false;
        assert!(!grant.is_valid(now_epoch_ms()));
    }

    #[test]
    fn grant_covers_action_and_resource() {
        let grant = AuthGrant::new(
            "g1",
            PrincipalId::agent("a1"),
            read_write_actions(),
            ResourceId::pane("p1"),
        );
        let now = now_epoch_ms();
        assert!(grant.covers(&AuthAction::Read, &ResourceId::pane("p1"), now));
        assert!(grant.covers(&AuthAction::Write, &ResourceId::pane("p1"), now));
        assert!(!grant.covers(&AuthAction::Delete, &ResourceId::pane("p1"), now));
        assert!(!grant.covers(&AuthAction::Read, &ResourceId::pane("p2"), now));
    }

    #[test]
    fn grant_wildcard_resource() {
        let grant = AuthGrant::new(
            "g1",
            PrincipalId::agent("a1"),
            read_action(),
            ResourceId::new(ResourceKind::Pane, "*"),
        );
        let now = now_epoch_ms();
        assert!(grant.covers(&AuthAction::Read, &ResourceId::pane("any"), now));
    }

    #[test]
    fn grant_fleet_covers_all() {
        let grant = AuthGrant::new(
            "g1",
            PrincipalId::human("admin"),
            all_basic_actions(),
            ResourceId::fleet(),
        );
        let now = now_epoch_ms();
        assert!(grant.covers(&AuthAction::Read, &ResourceId::pane("p1"), now));
        assert!(grant.covers(&AuthAction::Write, &ResourceId::session("s1"), now));
    }

    #[test]
    fn grant_subset_check() {
        let parent = AuthGrant::new(
            "parent",
            PrincipalId::human("admin"),
            all_basic_actions(),
            ResourceId::new(ResourceKind::Pane, "*"),
        );
        let child = AuthGrant::new(
            "child",
            PrincipalId::agent("a1"),
            read_action(),
            ResourceId::pane("p1"),
        );
        assert!(child.is_subset_of(&parent));

        let non_subset = AuthGrant::new(
            "ns",
            PrincipalId::agent("a1"),
            {
                let mut s = BTreeSet::new();
                s.insert(AuthAction::Delete);
                s
            },
            ResourceId::pane("p1"),
        );
        assert!(!non_subset.is_subset_of(&parent));
    }

    // ── AuthzDecision ──

    #[test]
    fn authz_decision_predicates() {
        let allow = AuthzDecision::Allow {
            grant_ids: vec!["g1".into()],
        };
        assert!(allow.is_allowed());
        assert!(!allow.is_denied());

        let deny = AuthzDecision::Deny {
            reason: "no".into(),
        };
        assert!(deny.is_denied());
        assert!(!deny.is_allowed());
    }

    #[test]
    fn authz_decision_display() {
        let allow = AuthzDecision::Allow {
            grant_ids: vec!["g1".into()],
        };
        assert!(allow.to_string().contains("g1"));

        let deny = AuthzDecision::Deny {
            reason: "no access".into(),
        };
        assert!(deny.to_string().contains("no access"));
    }

    #[test]
    fn authz_decision_serde_roundtrip() {
        let decisions = vec![
            AuthzDecision::Allow {
                grant_ids: vec!["g1".into()],
            },
            AuthzDecision::Deny {
                reason: "test".into(),
            },
            AuthzDecision::RequireApproval {
                approver: PrincipalId::human("admin"),
                reason: "needs approval".into(),
            },
        ];
        for d in decisions {
            let json = serde_json::to_string(&d).unwrap();
            let rt: AuthzDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(d, rt);
        }
    }

    // ── IdentityGraphError ──

    #[test]
    fn error_display() {
        let e = IdentityGraphError::PrincipalNotFound {
            id: "agent:x".into(),
        };
        assert!(e.to_string().contains("agent:x"));

        let e = IdentityGraphError::CircularDelegation {
            chain: vec!["a".into(), "b".into()],
        };
        assert!(e.to_string().contains("a -> b"));
    }

    // ── IdentityGraph: principal management ──

    #[test]
    fn register_and_query_principal() {
        let g = test_graph();
        assert!(g.is_registered(&PrincipalId::agent("claude-1")));
        assert!(!g.is_registered(&PrincipalId::agent("unknown")));
    }

    #[test]
    fn duplicate_principal_rejected() {
        let mut g = IdentityGraph::new();
        g.register_principal(PrincipalId::agent("a1")).unwrap();
        let result = g.register_principal(PrincipalId::agent("a1"));
        let is_dup = matches!(result, Err(IdentityGraphError::DuplicatePrincipal { .. }));
        assert!(is_dup);
    }

    #[test]
    fn trust_level_default_and_override() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("a1");
        g.register_principal(p.clone()).unwrap();
        assert_eq!(g.trust_level(&p), Some(TrustLevel::Standard));

        g.set_trust(&p, TrustLevel::High).unwrap();
        assert_eq!(g.trust_level(&p), Some(TrustLevel::High));
    }

    #[test]
    fn deactivate_principal_revokes_grants() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("a1");
        g.register_principal(p.clone()).unwrap();

        let grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        g.add_grant(grant).unwrap();
        assert_eq!(g.active_grant_count(), 1);

        g.deactivate_principal(&p).unwrap();
        assert!(!g.is_registered(&p));
        assert_eq!(g.active_grant_count(), 0);
    }

    // ── IdentityGraph: grant management ──

    #[test]
    fn add_and_query_grants() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        let grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        g.add_grant(grant).unwrap();

        let grants = g.grants_for(&p, now_epoch_ms());
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].grant_id, "g1");
    }

    #[test]
    fn duplicate_grant_rejected() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        let g1 = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        let g2 = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p2"));
        g.add_grant(g1).unwrap();
        let is_dup = matches!(
            g.add_grant(g2),
            Err(IdentityGraphError::DuplicateGrant { .. })
        );
        assert!(is_dup);
    }

    #[test]
    fn revoke_grant() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        let grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        g.add_grant(grant).unwrap();
        assert_eq!(g.active_grant_count(), 1);

        g.revoke_grant("g1").unwrap();
        assert_eq!(g.active_grant_count(), 0);
    }

    // ── IdentityGraph: authorization ──

    #[test]
    fn authorize_with_direct_grant() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        let grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        g.add_grant(grant).unwrap();

        let decision = g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_denied_no_grant() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");

        let decision = g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));
        assert!(decision.is_denied());
    }

    #[test]
    fn authorize_denied_wrong_action() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        let grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        g.add_grant(grant).unwrap();

        let decision = g.authorize(&p, &AuthAction::Write, &ResourceId::pane("p1"));
        assert!(decision.is_denied());
    }

    #[test]
    fn authorize_denied_wrong_resource() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        let grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        g.add_grant(grant).unwrap();

        let decision = g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p2"));
        assert!(decision.is_denied());
    }

    #[test]
    fn authorize_denied_unregistered_principal() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("unknown");

        let decision = g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));
        assert!(decision.is_denied());
    }

    #[test]
    fn authorize_denied_insufficient_trust() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::connector("c1"); // Low trust
        g.register_principal(p.clone()).unwrap();

        // Delete requires High trust
        let grant = AuthGrant::new(
            "g1",
            p.clone(),
            {
                let mut s = BTreeSet::new();
                s.insert(AuthAction::Delete);
                s
            },
            ResourceId::pane("p1"),
        );
        g.add_grant(grant).unwrap();

        let decision = g.authorize(&p, &AuthAction::Delete, &ResourceId::pane("p1"));
        assert!(decision.is_denied());
    }

    #[test]
    fn authorize_via_group() {
        let mut g = IdentityGraph::new();
        let group = PrincipalId::group("editors");
        let member = PrincipalId::agent("claude-1");
        g.register_principal(group.clone()).unwrap();
        g.register_principal(member.clone()).unwrap();

        // Grant to group
        let grant = AuthGrant::new(
            "g1",
            group.clone(),
            read_write_actions(),
            ResourceId::pane("p1"),
        );
        g.add_grant(grant).unwrap();

        // Add agent to group
        g.add_to_group(&group, &member).unwrap();

        // Agent should inherit group's grants
        let decision = g.authorize(&member, &AuthAction::Read, &ResourceId::pane("p1"));
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_via_delegation() {
        let mut g = IdentityGraph::new();
        let admin = PrincipalId::human("admin");
        let agent = PrincipalId::agent("claude-1");
        g.register_principal(admin.clone()).unwrap();
        g.register_principal(agent.clone()).unwrap();

        // Admin has broad grant
        let grant = AuthGrant::new(
            "g1",
            admin.clone(),
            all_basic_actions(),
            ResourceId::new(ResourceKind::Pane, "*"),
        );
        g.add_grant(grant).unwrap();

        // Delegate non-admin actions to agent
        let delegation = Delegation {
            delegation_id: "d1".into(),
            delegator: admin.clone(),
            delegate: agent.clone(),
            scope: DelegationScope::AllNonAdmin,
            active: true,
            created_at_ms: now_epoch_ms(),
            expires_at_ms: None,
        };
        g.add_delegation(delegation).unwrap();

        // Agent should be able to read via delegation
        let decision = g.authorize(&agent, &AuthAction::Read, &ResourceId::pane("p1"));
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_requires_approval() {
        let mut g = IdentityGraph::new();
        let agent = PrincipalId::agent("claude-1");
        let admin = PrincipalId::human("admin");
        g.register_principal(agent.clone()).unwrap();
        g.register_principal(admin.clone()).unwrap();

        let mut grant = AuthGrant::new(
            "g1",
            agent.clone(),
            {
                let mut s = BTreeSet::new();
                s.insert(AuthAction::Write);
                s
            },
            ResourceId::pane("p1"),
        );
        grant
            .conditions
            .push(GrantCondition::RequiresApproval(admin.clone()));
        g.add_grant(grant).unwrap();

        let decision = g.authorize(&agent, &AuthAction::Write, &ResourceId::pane("p1"));
        let is_approval = matches!(decision, AuthzDecision::RequireApproval { .. });
        assert!(is_approval);
    }

    // ── Delegation cycle detection ──

    #[test]
    fn delegation_self_loop_rejected() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("a1");
        g.register_principal(p.clone()).unwrap();

        let d = Delegation {
            delegation_id: "d1".into(),
            delegator: p.clone(),
            delegate: p.clone(),
            scope: DelegationScope::AllNonAdmin,
            active: true,
            created_at_ms: now_epoch_ms(),
            expires_at_ms: None,
        };
        let is_circular = matches!(
            g.add_delegation(d),
            Err(IdentityGraphError::CircularDelegation { .. })
        );
        assert!(is_circular);
    }

    // ── Group membership ──

    #[test]
    fn add_to_non_group_rejected() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("a1");
        let m = PrincipalId::agent("a2");
        g.register_principal(p.clone()).unwrap();
        g.register_principal(m.clone()).unwrap();

        let result = g.add_to_group(&p, &m);
        let is_not_group = matches!(result, Err(IdentityGraphError::NotAGroup { .. }));
        assert!(is_not_group);
    }

    #[test]
    fn groups_of_returns_active_only() {
        let mut g = IdentityGraph::new();
        let group = PrincipalId::group("g1");
        let member = PrincipalId::agent("a1");
        g.register_principal(group.clone()).unwrap();
        g.register_principal(member.clone()).unwrap();

        g.add_to_group(&group, &member).unwrap();
        let groups = g.groups_of(&member, now_epoch_ms());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], &group);
    }

    // ── Expire stale ──

    #[test]
    fn expire_stale_grants() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("a1");
        g.register_principal(p.clone()).unwrap();

        let mut grant = AuthGrant::new("g1", p.clone(), read_action(), ResourceId::pane("p1"));
        grant.expires_at_ms = Some(1000);
        grant.created_at_ms = 500;
        g.add_grant(grant).unwrap();

        assert_eq!(g.active_grant_count(), 1);
        let expired = g.expire_stale(2000);
        assert_eq!(expired, 1);
        assert_eq!(g.active_grant_count(), 0);
    }

    // ── Query helpers ──

    #[test]
    fn accessible_resources_query() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        g.add_grant(AuthGrant::new(
            "g1",
            p.clone(),
            read_action(),
            ResourceId::pane("p1"),
        ))
        .unwrap();
        g.add_grant(AuthGrant::new(
            "g2",
            p.clone(),
            read_action(),
            ResourceId::pane("p2"),
        ))
        .unwrap();

        let resources = g.accessible_resources(&p, &AuthAction::Read, now_epoch_ms());
        assert_eq!(resources.len(), 2);
    }

    #[test]
    fn who_can_access_query() {
        let mut g = test_graph();
        let pane = ResourceId::pane("p1");
        g.add_grant(AuthGrant::new(
            "g1",
            PrincipalId::agent("claude-1"),
            read_action(),
            pane.clone(),
        ))
        .unwrap();
        g.add_grant(AuthGrant::new(
            "g2",
            PrincipalId::human("operator-1"),
            read_action(),
            pane.clone(),
        ))
        .unwrap();

        let principals = g.who_can_access(&pane, &AuthAction::Read, now_epoch_ms());
        assert_eq!(principals.len(), 2);
    }

    // ── Audit log ──

    #[test]
    fn audit_log_records_decisions() {
        let mut g = test_graph();
        assert!(g.audit_log().is_empty());

        let p = PrincipalId::agent("claude-1");
        g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));

        assert_eq!(g.audit_log().len(), 1);
        assert!(g.audit_log()[0].decision.is_denied());
    }

    #[test]
    fn audit_log_bounded() {
        let mut g = IdentityGraph::new().with_audit_limit(3);
        let p = PrincipalId::agent("a1");
        g.register_principal(p.clone()).unwrap();

        for _ in 0..5 {
            g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));
        }

        assert_eq!(g.audit_log().len(), 3);
    }

    #[test]
    fn audit_log_json_serializes() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));

        let json = g.audit_log_json().unwrap();
        assert!(json.contains("claude-1"));
    }

    // ── Telemetry ──

    #[test]
    fn telemetry_tracks_queries() {
        let mut g = test_graph();
        let p = PrincipalId::agent("claude-1");
        g.authorize(&p, &AuthAction::Read, &ResourceId::pane("p1"));

        let t = g.telemetry();
        assert_eq!(t.authz_queries, 1);
        assert_eq!(t.authz_denied, 1);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let t = IdentityGraphTelemetry {
            principals_registered: 5,
            grants_active: 10,
            authz_queries: 100,
            authz_allowed: 80,
            authz_denied: 20,
            ..Default::default()
        };
        let json = serde_json::to_string(&t).unwrap();
        let rt: IdentityGraphTelemetry = serde_json::from_str(&json).unwrap();
        assert_eq!(t, rt);
    }

    // ── Summary ──

    #[test]
    fn summary_reflects_graph_state() {
        let g = test_graph();
        let s = g.summary();
        assert_eq!(s["principals"], 4);
        assert_eq!(s["grants"], 0);
    }

    // ── Grant condition serde ──

    #[test]
    fn grant_condition_serde_roundtrip() {
        let conditions = vec![
            GrantCondition::TimeWindow {
                start_ms: 1000,
                end_ms: 2000,
            },
            GrantCondition::MinTrust(TrustLevel::Standard),
            GrantCondition::Domain("prod".into()),
            GrantCondition::RequiresApproval(PrincipalId::human("admin")),
            GrantCondition::RateLimit {
                max_uses: 10,
                window_ms: 60_000,
            },
        ];
        for c in conditions {
            let json = serde_json::to_string(&c).unwrap();
            let rt: GrantCondition = serde_json::from_str(&json).unwrap();
            assert_eq!(c, rt);
        }
    }

    // ── DelegationScope serde ──

    #[test]
    fn delegation_scope_serde_roundtrip() {
        let scopes = vec![
            DelegationScope::Grants(vec!["g1".into(), "g2".into()]),
            DelegationScope::Resources(vec![ResourceId::pane("p1")]),
            DelegationScope::Actions(read_write_actions()),
            DelegationScope::AllNonAdmin,
        ];
        for s in scopes {
            let json = serde_json::to_string(&s).unwrap();
            let rt: DelegationScope = serde_json::from_str(&json).unwrap();
            assert_eq!(s, rt);
        }
    }

    // ── Principal with trust override ──

    #[test]
    fn register_with_explicit_trust() {
        let mut g = IdentityGraph::new();
        let p = PrincipalId::agent("a1");
        g.register_principal_with_trust(p.clone(), TrustLevel::Admin)
            .unwrap();
        assert_eq!(g.trust_level(&p), Some(TrustLevel::Admin));
    }

    // ── E2E: full authorization flow ──

    #[test]
    fn e2e_full_authorization_flow() {
        let mut g = IdentityGraph::new();

        // Register principals
        let admin = PrincipalId::human("admin");
        let group = PrincipalId::group("devs");
        let agent = PrincipalId::agent("claude-1");
        g.register_principal(admin.clone()).unwrap();
        g.register_principal(group.clone()).unwrap();
        g.register_principal(agent.clone()).unwrap();

        // Admin gets broad access
        g.add_grant(AuthGrant::new(
            "admin-all",
            admin.clone(),
            all_basic_actions(),
            ResourceId::fleet(),
        ))
        .unwrap();

        // Group gets read/write on panes
        g.add_grant(AuthGrant::new(
            "devs-panes",
            group.clone(),
            read_write_actions(),
            ResourceId::new(ResourceKind::Pane, "*"),
        ))
        .unwrap();

        // Agent joins group
        g.add_to_group(&group, &agent).unwrap();

        // Agent can read panes via group
        let d = g.authorize(&agent, &AuthAction::Read, &ResourceId::pane("p1"));
        assert!(d.is_allowed());

        // Agent can write panes via group
        let d = g.authorize(&agent, &AuthAction::Write, &ResourceId::pane("p1"));
        assert!(d.is_allowed());

        // Agent cannot create (not in group's grants)
        let d = g.authorize(&agent, &AuthAction::Create, &ResourceId::pane("p1"));
        assert!(d.is_denied());

        // Admin can do everything
        let d = g.authorize(&admin, &AuthAction::Create, &ResourceId::session("s1"));
        assert!(d.is_allowed());

        // Verify telemetry
        let t = g.telemetry();
        assert_eq!(t.authz_queries, 4);
        assert_eq!(t.authz_allowed, 3);
        assert_eq!(t.authz_denied, 1);
    }
}
