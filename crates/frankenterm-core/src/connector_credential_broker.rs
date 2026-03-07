//! Connector credential broker: policy-aware secret handoff with rotation and audit.
//!
//! Provides just-in-time, least-privilege scoped credentials to connectors via
//! secure handoff. Supports multiple secret provider backends, automatic rotation,
//! revocation, and explicit audit trails without secret leakage into logs or events.
//!
//! Part of ft-3681t.5.5.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

// =============================================================================
// Error types
// =============================================================================

/// Credential broker errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialBrokerError {
    #[error("provider not found: {provider_id}")]
    ProviderNotFound { provider_id: String },

    #[error("credential not found: {credential_id}")]
    CredentialNotFound { credential_id: String },

    #[error("connector not authorized: {connector_id} for scope {scope}")]
    NotAuthorized { connector_id: String, scope: String },

    #[error("credential expired: {credential_id}")]
    CredentialExpired { credential_id: String },

    #[error("credential revoked: {credential_id}")]
    CredentialRevoked { credential_id: String },

    #[error("provider unavailable: {provider_id}: {reason}")]
    ProviderUnavailable { provider_id: String, reason: String },

    #[error("rotation failed for {credential_id}: {reason}")]
    RotationFailed {
        credential_id: String,
        reason: String,
    },

    #[error("lease expired: {lease_id}")]
    LeaseExpired { lease_id: String },

    #[error("max active leases exceeded for {connector_id}: limit={limit}")]
    MaxLeasesExceeded { connector_id: String, limit: usize },
}

// =============================================================================
// Credential scope and type classification
// =============================================================================

/// Classification of a credential's sensitivity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSensitivity {
    /// Low-risk credentials (e.g., public API read keys).
    Low,
    /// Medium-risk credentials (e.g., write-scoped API keys).
    Medium,
    /// High-risk credentials (e.g., admin tokens, signing keys).
    High,
    /// Critical credentials (e.g., root access, key material).
    Critical,
}

impl std::fmt::Display for CredentialSensitivity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => f.write_str("low"),
            Self::Medium => f.write_str("medium"),
            Self::High => f.write_str("high"),
            Self::Critical => f.write_str("critical"),
        }
    }
}

/// Type of credential being managed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// API key or token.
    ApiKey,
    /// OAuth2 client credentials.
    OAuth2Client,
    /// OAuth2 access/refresh token pair.
    OAuth2Token,
    /// TLS client certificate.
    TlsCertificate,
    /// SSH key pair.
    SshKey,
    /// Symmetric encryption key.
    SymmetricKey,
    /// Generic secret blob.
    GenericSecret,
}

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey => f.write_str("api_key"),
            Self::OAuth2Client => f.write_str("oauth2_client"),
            Self::OAuth2Token => f.write_str("oauth2_token"),
            Self::TlsCertificate => f.write_str("tls_certificate"),
            Self::SshKey => f.write_str("ssh_key"),
            Self::SymmetricKey => f.write_str("symmetric_key"),
            Self::GenericSecret => f.write_str("generic_secret"),
        }
    }
}

/// Scope that a credential lease grants access to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialScope {
    /// The provider namespace (e.g., "github", "slack", "aws").
    pub provider: String,
    /// Resource path or pattern within the provider.
    pub resource: String,
    /// Allowed operations (e.g., "read", "write", "admin").
    pub operations: Vec<String>,
}

impl CredentialScope {
    #[must_use]
    pub fn new(
        provider: impl Into<String>,
        resource: impl Into<String>,
        operations: Vec<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            resource: resource.into(),
            operations,
        }
    }

    /// Check if this scope is a subset of `other` (i.e., `other` permits everything `self` needs).
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        if self.provider != other.provider {
            return false;
        }
        // Wildcard resource in `other` covers everything
        if other.resource == "*" {
            return self.operations.iter().all(|op| {
                other.operations.contains(op) || other.operations.contains(&"*".to_string())
            });
        }
        if self.resource != other.resource {
            return false;
        }
        self.operations
            .iter()
            .all(|op| other.operations.contains(op) || other.operations.contains(&"*".to_string()))
    }
}

// =============================================================================
// Secret provider model
// =============================================================================

/// State of a secret provider backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatus {
    Available,
    Degraded,
    Unavailable,
}

/// Configuration for a secret provider backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretProviderConfig {
    /// Unique provider identifier.
    pub provider_id: String,
    /// Display name.
    pub display_name: String,
    /// Provider type (e.g., "vault", "env", "keychain", "file").
    pub provider_type: String,
    /// Maximum concurrent credential leases from this provider.
    pub max_concurrent_leases: usize,
    /// Default TTL for leases (milliseconds).
    pub default_lease_ttl_ms: u64,
    /// Whether this provider supports rotation.
    pub supports_rotation: bool,
    /// Sensitivity ceiling — won't issue credentials above this level.
    pub max_sensitivity: CredentialSensitivity,
}

/// Runtime state of a registered secret provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretProvider {
    pub config: SecretProviderConfig,
    pub status: ProviderStatus,
    pub active_leases: u32,
    pub total_issued: u64,
    pub total_revoked: u64,
    pub total_rotations: u64,
    pub last_health_check_ms: u64,
    pub registered_at_ms: u64,
}

impl SecretProvider {
    #[must_use]
    pub fn new(config: SecretProviderConfig, now_ms: u64) -> Self {
        Self {
            config,
            status: ProviderStatus::Available,
            active_leases: 0,
            total_issued: 0,
            total_revoked: 0,
            total_rotations: 0,
            last_health_check_ms: now_ms,
            registered_at_ms: now_ms,
        }
    }
}

// =============================================================================
// Credential record and lease model
// =============================================================================

/// State of a credential in the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialState {
    /// Active and available for lease.
    Active,
    /// Being rotated — new version pending.
    Rotating,
    /// Revoked — no new leases, existing leases will be terminated.
    Revoked,
    /// Expired — TTL elapsed.
    Expired,
}

/// A managed credential record (never contains the actual secret value).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedCredential {
    /// Unique credential identifier.
    pub credential_id: String,
    /// Provider that manages this credential.
    pub provider_id: String,
    /// Kind of credential.
    pub kind: CredentialKind,
    /// Sensitivity level.
    pub sensitivity: CredentialSensitivity,
    /// Current state.
    pub state: CredentialState,
    /// Scopes this credential can serve.
    pub permitted_scopes: Vec<CredentialScope>,
    /// Version counter (incremented on rotation).
    pub version: u32,
    /// When this credential was created.
    pub created_at_ms: u64,
    /// When this credential expires (0 = never).
    pub expires_at_ms: u64,
    /// Last rotation timestamp.
    pub last_rotated_at_ms: u64,
    /// How many active leases reference this credential.
    pub active_lease_count: u32,
}

/// State of a credential lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    Active,
    Expired,
    Revoked,
}

/// A lease granting a connector time-limited access to a credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialLease {
    /// Unique lease identifier.
    pub lease_id: String,
    /// The credential being leased.
    pub credential_id: String,
    /// Connector receiving the credential.
    pub connector_id: String,
    /// Granted scope (may be narrower than credential's full scope).
    pub granted_scope: CredentialScope,
    /// Lease state.
    pub state: LeaseState,
    /// When the lease was issued.
    pub issued_at_ms: u64,
    /// When the lease expires.
    pub expires_at_ms: u64,
    /// Credential version at time of lease.
    pub credential_version: u32,
}

impl CredentialLease {
    /// Check if the lease is still valid at the given time.
    #[must_use]
    pub fn is_valid_at(&self, now_ms: u64) -> bool {
        self.state == LeaseState::Active && now_ms < self.expires_at_ms
    }
}

// =============================================================================
// Authorization policy for credential access
// =============================================================================

/// Policy rule governing which connectors can access which credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialAccessRule {
    /// Rule identifier.
    pub rule_id: String,
    /// Connector ID pattern (exact match or "*" for any).
    pub connector_pattern: String,
    /// Allowed credential scope.
    pub permitted_scope: CredentialScope,
    /// Maximum sensitivity level this rule permits.
    pub max_sensitivity: CredentialSensitivity,
    /// Maximum lease TTL override (ms, 0 = use provider default).
    pub max_lease_ttl_ms: u64,
    /// Maximum concurrent leases per connector under this rule.
    pub max_concurrent_leases: usize,
}

impl CredentialAccessRule {
    /// Check if this rule matches a given connector and requested scope.
    #[must_use]
    pub fn matches(&self, connector_id: &str, requested_scope: &CredentialScope) -> bool {
        let connector_matches =
            self.connector_pattern == "*" || self.connector_pattern == connector_id;
        let scope_matches = requested_scope.is_subset_of(&self.permitted_scope);
        connector_matches && scope_matches
    }
}

// =============================================================================
// Audit trail
// =============================================================================

/// Audit event for credential operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialAuditEvent {
    pub timestamp_ms: u64,
    pub event_type: CredentialAuditType,
    pub credential_id: String,
    pub connector_id: Option<String>,
    pub lease_id: Option<String>,
    pub detail: String,
}

/// Types of credential audit events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAuditType {
    CredentialRegistered,
    LeaseIssued,
    LeaseExpired,
    LeaseRevoked,
    CredentialRotated,
    CredentialRevoked,
    CredentialExpired,
    AccessDenied,
    ProviderRegistered,
    ProviderStatusChanged,
}

impl std::fmt::Display for CredentialAuditType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CredentialRegistered => f.write_str("credential_registered"),
            Self::LeaseIssued => f.write_str("lease_issued"),
            Self::LeaseExpired => f.write_str("lease_expired"),
            Self::LeaseRevoked => f.write_str("lease_revoked"),
            Self::CredentialRotated => f.write_str("credential_rotated"),
            Self::CredentialRevoked => f.write_str("credential_revoked"),
            Self::CredentialExpired => f.write_str("credential_expired"),
            Self::AccessDenied => f.write_str("access_denied"),
            Self::ProviderRegistered => f.write_str("provider_registered"),
            Self::ProviderStatusChanged => f.write_str("provider_status_changed"),
        }
    }
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for the credential broker.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialBrokerTelemetry {
    pub leases_issued: u64,
    pub leases_expired: u64,
    pub leases_revoked: u64,
    pub access_denied: u64,
    pub rotations_completed: u64,
    pub rotations_failed: u64,
    pub credentials_registered: u64,
    pub credentials_revoked: u64,
    pub providers_registered: u64,
}

/// Snapshot of broker telemetry (serializable for diagnostics).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialBrokerTelemetrySnapshot {
    pub captured_at_ms: u64,
    pub counters: CredentialBrokerTelemetry,
    pub active_leases: u32,
    pub active_credentials: u32,
    pub active_providers: u32,
}

// =============================================================================
// Credential broker
// =============================================================================

const MAX_AUDIT_EVENTS: usize = 1024;
const MAX_LEASES_PER_CONNECTOR_DEFAULT: usize = 10;

/// The credential broker: manages providers, credentials, leases, and access policy.
#[derive(Debug)]
pub struct ConnectorCredentialBroker {
    providers: BTreeMap<String, SecretProvider>,
    credentials: BTreeMap<String, ManagedCredential>,
    leases: BTreeMap<String, CredentialLease>,
    access_rules: Vec<CredentialAccessRule>,
    audit_log: VecDeque<CredentialAuditEvent>,
    telemetry: CredentialBrokerTelemetry,
    lease_counter: u64,
}

impl ConnectorCredentialBroker {
    /// Create a new credential broker.
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: BTreeMap::new(),
            credentials: BTreeMap::new(),
            leases: BTreeMap::new(),
            access_rules: Vec::new(),
            audit_log: VecDeque::new(),
            telemetry: CredentialBrokerTelemetry::default(),
            lease_counter: 0,
        }
    }

    // ---- Provider management ----

    /// Register a secret provider backend.
    pub fn register_provider(
        &mut self,
        config: SecretProviderConfig,
        now_ms: u64,
    ) -> Result<(), CredentialBrokerError> {
        let provider_id = config.provider_id.clone();
        let provider = SecretProvider::new(config, now_ms);
        self.providers.insert(provider_id.clone(), provider);
        self.telemetry.providers_registered += 1;
        self.emit_audit(CredentialAuditEvent {
            timestamp_ms: now_ms,
            event_type: CredentialAuditType::ProviderRegistered,
            credential_id: String::new(),
            connector_id: None,
            lease_id: None,
            detail: format!("provider {provider_id} registered"),
        });
        Ok(())
    }

    /// Update provider health status.
    pub fn update_provider_status(
        &mut self,
        provider_id: &str,
        status: ProviderStatus,
        now_ms: u64,
    ) -> Result<(), CredentialBrokerError> {
        let provider = self.providers.get_mut(provider_id).ok_or_else(|| {
            CredentialBrokerError::ProviderNotFound {
                provider_id: provider_id.to_string(),
            }
        })?;
        let old_status = provider.status;
        provider.status = status;
        provider.last_health_check_ms = now_ms;
        if old_status != status {
            self.emit_audit(CredentialAuditEvent {
                timestamp_ms: now_ms,
                event_type: CredentialAuditType::ProviderStatusChanged,
                credential_id: String::new(),
                connector_id: None,
                lease_id: None,
                detail: format!("provider {provider_id}: {old_status:?} -> {status:?}"),
            });
        }
        Ok(())
    }

    /// Get a provider by ID.
    #[must_use]
    pub fn get_provider(&self, provider_id: &str) -> Option<&SecretProvider> {
        self.providers.get(provider_id)
    }

    /// List all provider IDs.
    #[must_use]
    pub fn provider_ids(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    // ---- Credential management ----

    /// Register a credential (the broker never stores the actual secret value).
    pub fn register_credential(
        &mut self,
        credential: ManagedCredential,
        now_ms: u64,
    ) -> Result<(), CredentialBrokerError> {
        // Verify provider exists
        if !self.providers.contains_key(&credential.provider_id) {
            return Err(CredentialBrokerError::ProviderNotFound {
                provider_id: credential.provider_id.clone(),
            });
        }
        // Verify provider supports this sensitivity level
        let provider = &self.providers[&credential.provider_id];
        if credential.sensitivity > provider.config.max_sensitivity {
            return Err(CredentialBrokerError::NotAuthorized {
                connector_id: String::new(),
                scope: format!(
                    "sensitivity {} exceeds provider max {}",
                    credential.sensitivity, provider.config.max_sensitivity
                ),
            });
        }
        let cred_id = credential.credential_id.clone();
        self.credentials.insert(cred_id.clone(), credential);
        self.telemetry.credentials_registered += 1;
        self.emit_audit(CredentialAuditEvent {
            timestamp_ms: now_ms,
            event_type: CredentialAuditType::CredentialRegistered,
            credential_id: cred_id,
            connector_id: None,
            lease_id: None,
            detail: "credential registered".to_string(),
        });
        Ok(())
    }

    /// Get a credential record (no secret material).
    #[must_use]
    pub fn get_credential(&self, credential_id: &str) -> Option<&ManagedCredential> {
        self.credentials.get(credential_id)
    }

    // ---- Access rule management ----

    /// Add an access rule.
    pub fn add_access_rule(&mut self, rule: CredentialAccessRule) {
        self.access_rules.push(rule);
    }

    /// Check if a connector is authorized for a requested scope.
    #[must_use]
    pub fn is_authorized(
        &self,
        connector_id: &str,
        requested_scope: &CredentialScope,
        sensitivity: CredentialSensitivity,
    ) -> bool {
        self.access_rules.iter().any(|rule| {
            rule.matches(connector_id, requested_scope) && sensitivity <= rule.max_sensitivity
        })
    }

    // ---- Lease management ----

    /// Request a credential lease for a connector.
    pub fn request_lease(
        &mut self,
        connector_id: &str,
        credential_id: &str,
        requested_scope: CredentialScope,
        now_ms: u64,
    ) -> Result<CredentialLease, CredentialBrokerError> {
        // Verify credential exists and is active
        let credential = self.credentials.get(credential_id).ok_or_else(|| {
            CredentialBrokerError::CredentialNotFound {
                credential_id: credential_id.to_string(),
            }
        })?;

        match credential.state {
            CredentialState::Revoked => {
                return Err(CredentialBrokerError::CredentialRevoked {
                    credential_id: credential_id.to_string(),
                });
            }
            CredentialState::Expired => {
                return Err(CredentialBrokerError::CredentialExpired {
                    credential_id: credential_id.to_string(),
                });
            }
            CredentialState::Rotating | CredentialState::Active => {}
        }

        // Check expiration
        if credential.expires_at_ms > 0 && now_ms >= credential.expires_at_ms {
            return Err(CredentialBrokerError::CredentialExpired {
                credential_id: credential_id.to_string(),
            });
        }

        // Check authorization
        if !self.is_authorized(connector_id, &requested_scope, credential.sensitivity) {
            self.telemetry.access_denied += 1;
            self.emit_audit(CredentialAuditEvent {
                timestamp_ms: now_ms,
                event_type: CredentialAuditType::AccessDenied,
                credential_id: credential_id.to_string(),
                connector_id: Some(connector_id.to_string()),
                lease_id: None,
                detail: format!(
                    "connector {connector_id} denied access to {credential_id} for scope {}:{}",
                    requested_scope.provider, requested_scope.resource
                ),
            });
            return Err(CredentialBrokerError::NotAuthorized {
                connector_id: connector_id.to_string(),
                scope: format!("{}:{}", requested_scope.provider, requested_scope.resource),
            });
        }

        // Check per-connector lease limits
        let active_count = self
            .leases
            .values()
            .filter(|l| l.connector_id == connector_id && l.state == LeaseState::Active)
            .count();
        let max_leases = self
            .access_rules
            .iter()
            .filter(|r| r.matches(connector_id, &requested_scope))
            .map(|r| r.max_concurrent_leases)
            .max()
            .unwrap_or(MAX_LEASES_PER_CONNECTOR_DEFAULT);
        if active_count >= max_leases {
            return Err(CredentialBrokerError::MaxLeasesExceeded {
                connector_id: connector_id.to_string(),
                limit: max_leases,
            });
        }

        // Check provider is available
        let provider = self.providers.get(&credential.provider_id).ok_or_else(|| {
            CredentialBrokerError::ProviderNotFound {
                provider_id: credential.provider_id.clone(),
            }
        })?;
        if provider.status == ProviderStatus::Unavailable {
            return Err(CredentialBrokerError::ProviderUnavailable {
                provider_id: credential.provider_id.clone(),
                reason: "provider marked unavailable".to_string(),
            });
        }

        // Determine lease TTL
        let lease_ttl = self
            .access_rules
            .iter()
            .filter(|r| r.matches(connector_id, &requested_scope) && r.max_lease_ttl_ms > 0)
            .map(|r| r.max_lease_ttl_ms)
            .min()
            .unwrap_or(provider.config.default_lease_ttl_ms);

        // Issue the lease
        self.lease_counter += 1;
        let lease_id = format!("lease-{}", self.lease_counter);
        let lease = CredentialLease {
            lease_id: lease_id.clone(),
            credential_id: credential_id.to_string(),
            connector_id: connector_id.to_string(),
            granted_scope: requested_scope,
            state: LeaseState::Active,
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(lease_ttl),
            credential_version: credential.version,
        };

        // Update counters
        if let Some(cred) = self.credentials.get_mut(credential_id) {
            cred.active_lease_count += 1;
        }
        if let Some(prov) = self
            .providers
            .get_mut(&self.credentials[credential_id].provider_id)
        {
            prov.active_leases += 1;
            prov.total_issued += 1;
        }
        self.telemetry.leases_issued += 1;

        let result = lease.clone();
        self.leases.insert(lease_id.clone(), lease);

        self.emit_audit(CredentialAuditEvent {
            timestamp_ms: now_ms,
            event_type: CredentialAuditType::LeaseIssued,
            credential_id: credential_id.to_string(),
            connector_id: Some(connector_id.to_string()),
            lease_id: Some(lease_id),
            detail: format!(
                "lease issued, expires at {}",
                now_ms.saturating_add(lease_ttl)
            ),
        });

        Ok(result)
    }

    /// Revoke a specific lease.
    pub fn revoke_lease(
        &mut self,
        lease_id: &str,
        now_ms: u64,
    ) -> Result<(), CredentialBrokerError> {
        let lease =
            self.leases
                .get_mut(lease_id)
                .ok_or_else(|| CredentialBrokerError::LeaseExpired {
                    lease_id: lease_id.to_string(),
                })?;
        if lease.state != LeaseState::Active {
            return Ok(()); // Already revoked/expired
        }
        lease.state = LeaseState::Revoked;
        let credential_id = lease.credential_id.clone();
        let connector_id = lease.connector_id.clone();

        if let Some(cred) = self.credentials.get_mut(&credential_id) {
            cred.active_lease_count = cred.active_lease_count.saturating_sub(1);
        }
        if let Some(cred) = self.credentials.get(&credential_id) {
            if let Some(prov) = self.providers.get_mut(&cred.provider_id) {
                prov.active_leases = prov.active_leases.saturating_sub(1);
                prov.total_revoked += 1;
            }
        }
        self.telemetry.leases_revoked += 1;

        self.emit_audit(CredentialAuditEvent {
            timestamp_ms: now_ms,
            event_type: CredentialAuditType::LeaseRevoked,
            credential_id,
            connector_id: Some(connector_id),
            lease_id: Some(lease_id.to_string()),
            detail: "lease revoked".to_string(),
        });
        Ok(())
    }

    /// Expire all leases past their TTL.
    pub fn expire_leases(&mut self, now_ms: u64) -> Vec<String> {
        let mut expired = Vec::new();
        for (lease_id, lease) in &mut self.leases {
            if lease.state == LeaseState::Active && now_ms >= lease.expires_at_ms {
                lease.state = LeaseState::Expired;
                expired.push(lease_id.clone());
            }
        }
        for lease_id in &expired {
            let lease = &self.leases[lease_id];
            let credential_id = lease.credential_id.clone();
            let connector_id = lease.connector_id.clone();
            if let Some(cred) = self.credentials.get_mut(&credential_id) {
                cred.active_lease_count = cred.active_lease_count.saturating_sub(1);
            }
            if let Some(cred) = self.credentials.get(&credential_id) {
                if let Some(prov) = self.providers.get_mut(&cred.provider_id) {
                    prov.active_leases = prov.active_leases.saturating_sub(1);
                }
            }
            self.telemetry.leases_expired += 1;
            self.emit_audit(CredentialAuditEvent {
                timestamp_ms: now_ms,
                event_type: CredentialAuditType::LeaseExpired,
                credential_id,
                connector_id: Some(connector_id),
                lease_id: Some(lease_id.clone()),
                detail: "lease expired".to_string(),
            });
        }
        expired
    }

    // ---- Credential rotation ----

    /// Begin rotation of a credential. Marks it as Rotating and increments version.
    pub fn rotate_credential(
        &mut self,
        credential_id: &str,
        now_ms: u64,
    ) -> Result<u32, CredentialBrokerError> {
        let cred = self.credentials.get_mut(credential_id).ok_or_else(|| {
            CredentialBrokerError::CredentialNotFound {
                credential_id: credential_id.to_string(),
            }
        })?;
        if cred.state == CredentialState::Revoked {
            return Err(CredentialBrokerError::CredentialRevoked {
                credential_id: credential_id.to_string(),
            });
        }
        cred.state = CredentialState::Rotating;
        cred.version += 1;
        cred.last_rotated_at_ms = now_ms;
        let new_version = cred.version;
        let provider_id = cred.provider_id.clone();

        if let Some(prov) = self.providers.get_mut(&provider_id) {
            prov.total_rotations += 1;
        }
        self.telemetry.rotations_completed += 1;

        self.emit_audit(CredentialAuditEvent {
            timestamp_ms: now_ms,
            event_type: CredentialAuditType::CredentialRotated,
            credential_id: credential_id.to_string(),
            connector_id: None,
            lease_id: None,
            detail: format!("rotated to version {new_version}"),
        });

        Ok(new_version)
    }

    /// Complete rotation: set credential back to Active.
    pub fn complete_rotation(&mut self, credential_id: &str) -> Result<(), CredentialBrokerError> {
        let cred = self.credentials.get_mut(credential_id).ok_or_else(|| {
            CredentialBrokerError::CredentialNotFound {
                credential_id: credential_id.to_string(),
            }
        })?;
        if cred.state == CredentialState::Rotating {
            cred.state = CredentialState::Active;
        }
        Ok(())
    }

    // ---- Credential revocation ----

    /// Revoke a credential and all its active leases.
    pub fn revoke_credential(
        &mut self,
        credential_id: &str,
        now_ms: u64,
    ) -> Result<Vec<String>, CredentialBrokerError> {
        let cred = self.credentials.get_mut(credential_id).ok_or_else(|| {
            CredentialBrokerError::CredentialNotFound {
                credential_id: credential_id.to_string(),
            }
        })?;
        cred.state = CredentialState::Revoked;
        cred.active_lease_count = 0;
        self.telemetry.credentials_revoked += 1;

        // Revoke all active leases for this credential
        let mut revoked_leases = Vec::new();
        for (lease_id, lease) in &mut self.leases {
            if lease.credential_id == credential_id && lease.state == LeaseState::Active {
                lease.state = LeaseState::Revoked;
                revoked_leases.push(lease_id.clone());
                self.telemetry.leases_revoked += 1;
            }
        }

        self.emit_audit(CredentialAuditEvent {
            timestamp_ms: now_ms,
            event_type: CredentialAuditType::CredentialRevoked,
            credential_id: credential_id.to_string(),
            connector_id: None,
            lease_id: None,
            detail: format!(
                "credential revoked, {} leases terminated",
                revoked_leases.len()
            ),
        });

        Ok(revoked_leases)
    }

    // ---- Query helpers ----

    /// List active leases for a connector.
    #[must_use]
    pub fn active_leases_for_connector(&self, connector_id: &str) -> Vec<&CredentialLease> {
        self.leases
            .values()
            .filter(|l| l.connector_id == connector_id && l.state == LeaseState::Active)
            .collect()
    }

    /// List active leases for a credential.
    #[must_use]
    pub fn active_leases_for_credential(&self, credential_id: &str) -> Vec<&CredentialLease> {
        self.leases
            .values()
            .filter(|l| l.credential_id == credential_id && l.state == LeaseState::Active)
            .collect()
    }

    /// Get broker telemetry snapshot.
    #[must_use]
    pub fn telemetry_snapshot(&self, now_ms: u64) -> CredentialBrokerTelemetrySnapshot {
        CredentialBrokerTelemetrySnapshot {
            captured_at_ms: now_ms,
            counters: self.telemetry.clone(),
            active_leases: self
                .leases
                .values()
                .filter(|l| l.state == LeaseState::Active)
                .count() as u32,
            active_credentials: self
                .credentials
                .values()
                .filter(|c| c.state == CredentialState::Active)
                .count() as u32,
            active_providers: self
                .providers
                .values()
                .filter(|p| p.status == ProviderStatus::Available)
                .count() as u32,
        }
    }

    /// Get audit log entries.
    #[must_use]
    pub fn audit_log(&self) -> &VecDeque<CredentialAuditEvent> {
        &self.audit_log
    }

    /// Get all credential IDs.
    #[must_use]
    pub fn credential_ids(&self) -> Vec<String> {
        self.credentials.keys().cloned().collect()
    }

    // ---- Internal helpers ----

    fn emit_audit(&mut self, event: CredentialAuditEvent) {
        if self.audit_log.len() >= MAX_AUDIT_EVENTS {
            self.audit_log.pop_front();
        }
        self.audit_log.push_back(event);
    }
}

impl Default for ConnectorCredentialBroker {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn test_provider_config(id: &str) -> SecretProviderConfig {
        SecretProviderConfig {
            provider_id: id.to_string(),
            display_name: format!("Test Provider {id}"),
            provider_type: "vault".to_string(),
            max_concurrent_leases: 100,
            default_lease_ttl_ms: 3600_000,
            supports_rotation: true,
            max_sensitivity: CredentialSensitivity::Critical,
        }
    }

    fn test_credential(cred_id: &str, provider_id: &str) -> ManagedCredential {
        ManagedCredential {
            credential_id: cred_id.to_string(),
            provider_id: provider_id.to_string(),
            kind: CredentialKind::ApiKey,
            sensitivity: CredentialSensitivity::Medium,
            state: CredentialState::Active,
            permitted_scopes: vec![CredentialScope::new(
                "github",
                "repos/*",
                vec!["read".into(), "write".into()],
            )],
            version: 1,
            created_at_ms: 1000,
            expires_at_ms: 0,
            last_rotated_at_ms: 0,
            active_lease_count: 0,
        }
    }

    fn test_access_rule(connector_pattern: &str) -> CredentialAccessRule {
        CredentialAccessRule {
            rule_id: format!("rule-{connector_pattern}"),
            connector_pattern: connector_pattern.to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".into()]),
            max_sensitivity: CredentialSensitivity::High,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 5,
        }
    }

    fn setup_broker() -> ConnectorCredentialBroker {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("vault-1"), 1000)
            .unwrap();
        broker
            .register_credential(test_credential("cred-1", "vault-1"), 1000)
            .unwrap();
        broker.add_access_rule(test_access_rule("*"));
        broker
    }

    // ---- Provider tests ----

    #[test]
    fn register_provider_success() {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("p1"), 100)
            .unwrap();
        assert_eq!(broker.provider_ids(), vec!["p1"]);
        assert_eq!(broker.telemetry.providers_registered, 1);
    }

    #[test]
    fn update_provider_status() {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("p1"), 100)
            .unwrap();
        broker
            .update_provider_status("p1", ProviderStatus::Degraded, 200)
            .unwrap();
        assert_eq!(
            broker.get_provider("p1").unwrap().status,
            ProviderStatus::Degraded
        );
    }

    #[test]
    fn update_nonexistent_provider_fails() {
        let mut broker = ConnectorCredentialBroker::new();
        let err = broker
            .update_provider_status("ghost", ProviderStatus::Available, 100)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::ProviderNotFound { .. }
        ));
    }

    // ---- Credential registration tests ----

    #[test]
    fn register_credential_success() {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("v1"), 100)
            .unwrap();
        broker
            .register_credential(test_credential("c1", "v1"), 100)
            .unwrap();
        assert!(broker.get_credential("c1").is_some());
        assert_eq!(broker.telemetry.credentials_registered, 1);
    }

    #[test]
    fn register_credential_unknown_provider_fails() {
        let mut broker = ConnectorCredentialBroker::new();
        let err = broker
            .register_credential(test_credential("c1", "ghost"), 100)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::ProviderNotFound { .. }
        ));
    }

    #[test]
    fn register_credential_exceeding_sensitivity_fails() {
        let mut broker = ConnectorCredentialBroker::new();
        let mut config = test_provider_config("low-sec");
        config.max_sensitivity = CredentialSensitivity::Low;
        broker.register_provider(config, 100).unwrap();
        let err = broker
            .register_credential(test_credential("c1", "low-sec"), 100)
            .unwrap_err();
        assert!(matches!(err, CredentialBrokerError::NotAuthorized { .. }));
    }

    // ---- Access rule and authorization tests ----

    #[test]
    fn authorization_with_matching_rule() {
        let broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        assert!(broker.is_authorized("any-connector", &scope, CredentialSensitivity::Medium));
    }

    #[test]
    fn authorization_denied_for_excess_sensitivity() {
        let broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        // Rule max is High, Critical should be denied
        assert!(!broker.is_authorized("any-connector", &scope, CredentialSensitivity::Critical));
    }

    #[test]
    fn authorization_denied_for_unmatched_scope() {
        let mut broker = ConnectorCredentialBroker::new();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "narrow".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new("github", "repos/specific", vec!["read".into()]),
            max_sensitivity: CredentialSensitivity::High,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 5,
        });
        let scope = CredentialScope::new("slack", "channels", vec!["write".into()]);
        assert!(!broker.is_authorized("conn-1", &scope, CredentialSensitivity::Low));
    }

    // ---- Lease tests ----

    #[test]
    fn request_lease_success() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let lease = broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap();
        assert_eq!(lease.connector_id, "conn-1");
        assert_eq!(lease.credential_id, "cred-1");
        assert_eq!(lease.state, LeaseState::Active);
        assert!(lease.expires_at_ms > 2000);
        assert_eq!(broker.telemetry.leases_issued, 1);
    }

    #[test]
    fn request_lease_unknown_credential_fails() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let err = broker
            .request_lease("conn-1", "ghost", scope, 2000)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::CredentialNotFound { .. }
        ));
    }

    #[test]
    fn request_lease_revoked_credential_fails() {
        let mut broker = setup_broker();
        broker.revoke_credential("cred-1", 1500).unwrap();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let err = broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::CredentialRevoked { .. }
        ));
    }

    #[test]
    fn request_lease_expired_credential_fails() {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("v1"), 100)
            .unwrap();
        let mut cred = test_credential("c1", "v1");
        cred.expires_at_ms = 5000;
        broker.register_credential(cred, 100).unwrap();
        broker.add_access_rule(test_access_rule("*"));
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let err = broker
            .request_lease("conn-1", "c1", scope, 6000)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::CredentialExpired { .. }
        ));
    }

    #[test]
    fn request_lease_unauthorized_connector_fails() {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("v1"), 100)
            .unwrap();
        broker
            .register_credential(test_credential("c1", "v1"), 100)
            .unwrap();
        // No access rules → no authorization
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let err = broker
            .request_lease("conn-1", "c1", scope, 2000)
            .unwrap_err();
        assert!(matches!(err, CredentialBrokerError::NotAuthorized { .. }));
        assert_eq!(broker.telemetry.access_denied, 1);
    }

    #[test]
    fn request_lease_max_leases_exceeded() {
        let mut broker = ConnectorCredentialBroker::new();
        broker
            .register_provider(test_provider_config("v1"), 100)
            .unwrap();
        broker
            .register_credential(test_credential("c1", "v1"), 100)
            .unwrap();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "tight".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".into()]),
            max_sensitivity: CredentialSensitivity::High,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 2,
        });
        let scope = || CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker.request_lease("conn-1", "c1", scope(), 2000).unwrap();
        broker.request_lease("conn-1", "c1", scope(), 2001).unwrap();
        let err = broker
            .request_lease("conn-1", "c1", scope(), 2002)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::MaxLeasesExceeded { .. }
        ));
    }

    #[test]
    fn request_lease_unavailable_provider_fails() {
        let mut broker = setup_broker();
        broker
            .update_provider_status("vault-1", ProviderStatus::Unavailable, 1500)
            .unwrap();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let err = broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::ProviderUnavailable { .. }
        ));
    }

    // ---- Lease revocation and expiry ----

    #[test]
    fn revoke_lease_success() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let lease = broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap();
        broker.revoke_lease(&lease.lease_id, 3000).unwrap();
        assert_eq!(broker.leases[&lease.lease_id].state, LeaseState::Revoked);
        assert_eq!(broker.telemetry.leases_revoked, 1);
        // Lease count decremented
        assert_eq!(
            broker.get_credential("cred-1").unwrap().active_lease_count,
            0
        );
    }

    #[test]
    fn expire_leases_past_ttl() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let lease = broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap();
        let far_future = lease.expires_at_ms + 1;
        let expired = broker.expire_leases(far_future);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0], lease.lease_id);
        assert_eq!(broker.telemetry.leases_expired, 1);
    }

    #[test]
    fn expire_leases_not_yet_due() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap();
        let expired = broker.expire_leases(2001);
        assert!(expired.is_empty());
    }

    // ---- Rotation tests ----

    #[test]
    fn rotate_credential_increments_version() {
        let mut broker = setup_broker();
        let new_version = broker.rotate_credential("cred-1", 5000).unwrap();
        assert_eq!(new_version, 2);
        assert_eq!(
            broker.get_credential("cred-1").unwrap().state,
            CredentialState::Rotating
        );
        assert_eq!(broker.telemetry.rotations_completed, 1);
    }

    #[test]
    fn complete_rotation_returns_to_active() {
        let mut broker = setup_broker();
        broker.rotate_credential("cred-1", 5000).unwrap();
        broker.complete_rotation("cred-1").unwrap();
        assert_eq!(
            broker.get_credential("cred-1").unwrap().state,
            CredentialState::Active
        );
    }

    #[test]
    fn rotate_revoked_credential_fails() {
        let mut broker = setup_broker();
        broker.revoke_credential("cred-1", 5000).unwrap();
        let err = broker.rotate_credential("cred-1", 6000).unwrap_err();
        assert!(matches!(
            err,
            CredentialBrokerError::CredentialRevoked { .. }
        ));
    }

    // ---- Credential revocation tests ----

    #[test]
    fn revoke_credential_terminates_all_leases() {
        let mut broker = setup_broker();
        let scope = || CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker
            .request_lease("conn-1", "cred-1", scope(), 2000)
            .unwrap();
        broker
            .request_lease("conn-2", "cred-1", scope(), 2001)
            .unwrap();
        let revoked = broker.revoke_credential("cred-1", 3000).unwrap();
        assert_eq!(revoked.len(), 2);
        assert_eq!(
            broker.get_credential("cred-1").unwrap().state,
            CredentialState::Revoked
        );
        assert_eq!(broker.telemetry.credentials_revoked, 1);
        assert_eq!(broker.telemetry.leases_revoked, 2);
    }

    // ---- Query helper tests ----

    #[test]
    fn active_leases_for_connector() {
        let mut broker = setup_broker();
        let scope = || CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker
            .request_lease("conn-1", "cred-1", scope(), 2000)
            .unwrap();
        broker
            .request_lease("conn-2", "cred-1", scope(), 2001)
            .unwrap();
        assert_eq!(broker.active_leases_for_connector("conn-1").len(), 1);
        assert_eq!(broker.active_leases_for_connector("conn-2").len(), 1);
        assert_eq!(broker.active_leases_for_connector("conn-3").len(), 0);
    }

    #[test]
    fn active_leases_for_credential() {
        let mut broker = setup_broker();
        let scope = || CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker
            .request_lease("conn-1", "cred-1", scope(), 2000)
            .unwrap();
        broker
            .request_lease("conn-2", "cred-1", scope(), 2001)
            .unwrap();
        assert_eq!(broker.active_leases_for_credential("cred-1").len(), 2);
    }

    // ---- Telemetry tests ----

    #[test]
    fn telemetry_snapshot_reflects_state() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap();
        let snap = broker.telemetry_snapshot(3000);
        assert_eq!(snap.active_leases, 1);
        assert_eq!(snap.active_credentials, 1);
        assert_eq!(snap.active_providers, 1);
        assert_eq!(snap.counters.leases_issued, 1);
    }

    #[test]
    fn telemetry_serde_roundtrip() {
        let snap = CredentialBrokerTelemetrySnapshot {
            captured_at_ms: 1000,
            counters: CredentialBrokerTelemetry {
                leases_issued: 5,
                leases_expired: 2,
                leases_revoked: 1,
                access_denied: 3,
                rotations_completed: 1,
                rotations_failed: 0,
                credentials_registered: 4,
                credentials_revoked: 1,
                providers_registered: 2,
            },
            active_leases: 2,
            active_credentials: 3,
            active_providers: 2,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: CredentialBrokerTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    // ---- Audit log tests ----

    #[test]
    fn audit_log_records_events() {
        let mut broker = setup_broker();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        broker
            .request_lease("conn-1", "cred-1", scope, 2000)
            .unwrap();
        // Provider registered + credential registered + lease issued = 3
        assert_eq!(broker.audit_log().len(), 3);
    }

    #[test]
    fn audit_log_bounded() {
        let mut broker = ConnectorCredentialBroker::new();
        for i in 0..MAX_AUDIT_EVENTS + 100 {
            broker
                .register_provider(
                    SecretProviderConfig {
                        provider_id: format!("p{i}"),
                        display_name: format!("P{i}"),
                        provider_type: "env".to_string(),
                        max_concurrent_leases: 10,
                        default_lease_ttl_ms: 60_000,
                        supports_rotation: false,
                        max_sensitivity: CredentialSensitivity::Low,
                    },
                    i as u64,
                )
                .unwrap();
        }
        assert!(broker.audit_log().len() <= MAX_AUDIT_EVENTS);
    }

    // ---- Scope tests ----

    #[test]
    fn scope_subset_matching() {
        let wide = CredentialScope::new("github", "*", vec!["*".into()]);
        let narrow = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        assert!(narrow.is_subset_of(&wide));
        assert!(!wide.is_subset_of(&narrow));
    }

    #[test]
    fn scope_different_providers_not_subset() {
        let a = CredentialScope::new("github", "*", vec!["*".into()]);
        let b = CredentialScope::new("slack", "*", vec!["*".into()]);
        assert!(!a.is_subset_of(&b));
    }

    // ---- Sensitivity ordering ----

    #[test]
    fn sensitivity_ordering() {
        assert!(CredentialSensitivity::Low < CredentialSensitivity::Medium);
        assert!(CredentialSensitivity::Medium < CredentialSensitivity::High);
        assert!(CredentialSensitivity::High < CredentialSensitivity::Critical);
    }

    #[test]
    fn sensitivity_display() {
        assert_eq!(CredentialSensitivity::Low.to_string(), "low");
        assert_eq!(CredentialSensitivity::Critical.to_string(), "critical");
    }

    // ---- CredentialKind display ----

    #[test]
    fn credential_kind_display() {
        assert_eq!(CredentialKind::ApiKey.to_string(), "api_key");
        assert_eq!(CredentialKind::OAuth2Token.to_string(), "oauth2_token");
        assert_eq!(
            CredentialKind::TlsCertificate.to_string(),
            "tls_certificate"
        );
    }

    // ---- Lease validity ----

    #[test]
    fn lease_validity_check() {
        let lease = CredentialLease {
            lease_id: "l1".to_string(),
            credential_id: "c1".to_string(),
            connector_id: "conn-1".to_string(),
            granted_scope: CredentialScope::new("github", "repos/foo", vec!["read".into()]),
            state: LeaseState::Active,
            issued_at_ms: 1000,
            expires_at_ms: 5000,
            credential_version: 1,
        };
        assert!(lease.is_valid_at(2000));
        assert!(lease.is_valid_at(4999));
        assert!(!lease.is_valid_at(5000));
        assert!(!lease.is_valid_at(6000));
    }

    #[test]
    fn revoked_lease_not_valid() {
        let lease = CredentialLease {
            lease_id: "l1".to_string(),
            credential_id: "c1".to_string(),
            connector_id: "conn-1".to_string(),
            granted_scope: CredentialScope::new("github", "repos/foo", vec!["read".into()]),
            state: LeaseState::Revoked,
            issued_at_ms: 1000,
            expires_at_ms: 5000,
            credential_version: 1,
        };
        assert!(!lease.is_valid_at(2000));
    }

    // ---- Error display ----

    #[test]
    fn error_display_messages() {
        let e = CredentialBrokerError::ProviderNotFound {
            provider_id: "x".into(),
        };
        assert_eq!(e.to_string(), "provider not found: x");

        let e = CredentialBrokerError::CredentialRevoked {
            credential_id: "c1".into(),
        };
        assert_eq!(e.to_string(), "credential revoked: c1");

        let e = CredentialBrokerError::MaxLeasesExceeded {
            connector_id: "conn".into(),
            limit: 5,
        };
        assert_eq!(
            e.to_string(),
            "max active leases exceeded for conn: limit=5"
        );
    }

    // ---- Default impl ----

    #[test]
    fn default_broker() {
        let broker = ConnectorCredentialBroker::default();
        assert!(broker.provider_ids().is_empty());
        assert!(broker.credential_ids().is_empty());
    }

    // ---- Audit type display ----

    #[test]
    fn audit_type_display() {
        assert_eq!(CredentialAuditType::LeaseIssued.to_string(), "lease_issued");
        assert_eq!(
            CredentialAuditType::CredentialRotated.to_string(),
            "credential_rotated"
        );
        assert_eq!(
            CredentialAuditType::AccessDenied.to_string(),
            "access_denied"
        );
    }

    // ---- Degraded provider still allows leases ----

    #[test]
    fn degraded_provider_allows_leases() {
        let mut broker = setup_broker();
        broker
            .update_provider_status("vault-1", ProviderStatus::Degraded, 1500)
            .unwrap();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let lease = broker.request_lease("conn-1", "cred-1", scope, 2000);
        assert!(lease.is_ok());
    }

    // ---- Rotating credential allows leases ----

    #[test]
    fn rotating_credential_allows_leases() {
        let mut broker = setup_broker();
        broker.rotate_credential("cred-1", 1500).unwrap();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".into()]);
        let lease = broker.request_lease("conn-1", "cred-1", scope, 2000);
        assert!(lease.is_ok());
    }
}
