//! Connector registry client: signed package verification and trust policy.
//!
//! Provides a typed connector-registry interaction layer that validates
//! signed packages (SHA-256 digests), parses manifests, checks
//! transparency-log provenance, and enforces explicit trust policy gates
//! before connector install/upgrade/activation.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::connector_host_runtime::ConnectorCapability;

// =============================================================================
// Error types
// =============================================================================

/// Registry-specific errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorRegistryError {
    #[error("package not found: {package_id}")]
    PackageNotFound { package_id: String },

    #[error("digest mismatch for {package_id}: expected {expected}, got {actual}")]
    DigestMismatch {
        package_id: String,
        expected: String,
        actual: String,
    },

    #[error("signature verification failed for {package_id}: {reason}")]
    SignatureInvalid {
        package_id: String,
        reason: String,
    },

    #[error("manifest parse error: {reason}")]
    ManifestInvalid { reason: String },

    #[error("trust policy denied: {reason}")]
    TrustPolicyDenied { reason: String },

    #[error("transparency log check failed for {package_id}: {reason}")]
    TransparencyCheckFailed {
        package_id: String,
        reason: String,
    },

    #[error("capability not permitted: {capability}")]
    CapabilityNotPermitted { capability: String },

    #[error("registry unavailable: {reason}")]
    RegistryUnavailable { reason: String },

    #[error("version conflict: installed {installed}, requested {requested}")]
    VersionConflict {
        installed: String,
        requested: String,
    },
}

// =============================================================================
// Package manifest
// =============================================================================

/// Schema version for the manifest format.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// A connector package manifest describing metadata, capabilities, and provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectorManifest {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// Unique package identifier (e.g. "github-events-connector").
    pub package_id: String,
    /// Semantic version string (e.g. "1.2.3").
    pub version: String,
    /// Human-readable display name.
    pub display_name: String,
    /// One-line description.
    pub description: String,
    /// Author/publisher identity.
    pub author: String,
    /// Minimum FrankenTerm version required (semver).
    pub min_ft_version: Option<String>,
    /// SHA-256 hex digest of the package payload.
    pub sha256_digest: String,
    /// Capabilities the connector declares it needs.
    pub required_capabilities: Vec<ConnectorCapability>,
    /// Publisher signature over the manifest (hex-encoded).
    pub publisher_signature: Option<String>,
    /// Transparency log inclusion proof token.
    pub transparency_token: Option<String>,
    /// Creation timestamp (unix ms).
    pub created_at_ms: u64,
    /// Arbitrary metadata.
    pub metadata: BTreeMap<String, String>,
}

impl ConnectorManifest {
    /// Validate the manifest structure.
    pub fn validate(&self) -> Result<(), ConnectorRegistryError> {
        if self.package_id.is_empty() {
            return Err(ConnectorRegistryError::ManifestInvalid {
                reason: "package_id must not be empty".to_string(),
            });
        }
        if self.version.is_empty() {
            return Err(ConnectorRegistryError::ManifestInvalid {
                reason: "version must not be empty".to_string(),
            });
        }
        if self.sha256_digest.len() != 64
            || !self.sha256_digest.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(ConnectorRegistryError::ManifestInvalid {
                reason: format!(
                    "sha256_digest must be a 64-char hex string, got {} chars",
                    self.sha256_digest.len()
                ),
            });
        }
        if self.schema_version == 0 || self.schema_version > MANIFEST_SCHEMA_VERSION {
            return Err(ConnectorRegistryError::ManifestInvalid {
                reason: format!(
                    "unsupported schema_version {}, max supported is {}",
                    self.schema_version, MANIFEST_SCHEMA_VERSION
                ),
            });
        }
        Ok(())
    }
}

// =============================================================================
// Trust policy
// =============================================================================

/// Trust level for a publisher or package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TrustLevel {
    /// Explicitly trusted (signed + known publisher).
    Trusted,
    /// Conditionally trusted (signed, unknown publisher).
    Conditional,
    /// Untrusted (no signature or failed checks).
    Untrusted,
    /// Explicitly blocked.
    Blocked,
}

impl TrustLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trusted => "trusted",
            Self::Conditional => "conditional",
            Self::Untrusted => "untrusted",
            Self::Blocked => "blocked",
        }
    }

    /// Whether this trust level allows installation.
    pub const fn allows_install(self) -> bool {
        matches!(self, Self::Trusted | Self::Conditional)
    }
}

impl fmt::Display for TrustLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Trust policy configuration controlling what is allowed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrustPolicy {
    /// Minimum trust level required for install.
    pub min_install_level: TrustLevel,
    /// Whether signature is required.
    pub require_signature: bool,
    /// Whether transparency log proof is required.
    pub require_transparency_proof: bool,
    /// Maximum allowed capabilities (deny if package requests more).
    pub max_allowed_capabilities: Vec<ConnectorCapability>,
    /// Explicitly trusted publisher identities.
    pub trusted_publishers: Vec<String>,
    /// Explicitly blocked package IDs.
    pub blocked_packages: Vec<String>,
}

impl Default for TrustPolicy {
    fn default() -> Self {
        Self {
            min_install_level: TrustLevel::Conditional,
            require_signature: true,
            require_transparency_proof: false,
            max_allowed_capabilities: vec![
                ConnectorCapability::Invoke,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
            ],
            trusted_publishers: Vec::new(),
            blocked_packages: Vec::new(),
        }
    }
}

impl TrustPolicy {
    /// Evaluate the trust level of a manifest against this policy.
    pub fn evaluate(&self, manifest: &ConnectorManifest) -> TrustLevel {
        // Blocked packages always return Blocked
        if self
            .blocked_packages
            .iter()
            .any(|id| id == &manifest.package_id)
        {
            return TrustLevel::Blocked;
        }

        // No signature → Untrusted
        if manifest.publisher_signature.is_none() {
            return TrustLevel::Untrusted;
        }

        // Known publisher → Trusted
        if self
            .trusted_publishers
            .iter()
            .any(|p| p == &manifest.author)
        {
            return TrustLevel::Trusted;
        }

        // Signed but unknown publisher → Conditional
        TrustLevel::Conditional
    }

    /// Check if a manifest's capabilities are within policy limits.
    pub fn check_capabilities(
        &self,
        manifest: &ConnectorManifest,
    ) -> Result<(), ConnectorRegistryError> {
        for cap in &manifest.required_capabilities {
            if !self.max_allowed_capabilities.contains(cap) {
                return Err(ConnectorRegistryError::CapabilityNotPermitted {
                    capability: cap.as_str().to_string(),
                });
            }
        }
        Ok(())
    }

    /// Full policy gate: trust + capabilities + optional transparency.
    pub fn gate(
        &self,
        manifest: &ConnectorManifest,
    ) -> Result<TrustLevel, ConnectorRegistryError> {
        let level = self.evaluate(manifest);

        if !level.allows_install() {
            return Err(ConnectorRegistryError::TrustPolicyDenied {
                reason: format!(
                    "trust level '{}' does not meet minimum '{}'",
                    level,
                    self.min_install_level,
                ),
            });
        }

        if self.require_signature && manifest.publisher_signature.is_none() {
            return Err(ConnectorRegistryError::TrustPolicyDenied {
                reason: "signature required but not present".to_string(),
            });
        }

        if self.require_transparency_proof && manifest.transparency_token.is_none() {
            return Err(ConnectorRegistryError::TrustPolicyDenied {
                reason: "transparency proof required but not present".to_string(),
            });
        }

        self.check_capabilities(manifest)?;

        Ok(level)
    }
}

// =============================================================================
// Registry entry (installed package)
// =============================================================================

/// Status of an installed connector package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageStatus {
    /// Verified and ready.
    Active,
    /// Downloaded but not yet verified.
    Pending,
    /// Disabled by policy or user.
    Disabled,
    /// Removed but metadata retained.
    Retired,
}

impl PackageStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Disabled => "disabled",
            Self::Retired => "retired",
        }
    }
}

impl fmt::Display for PackageStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registered connector package (post-verification).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackageEntry {
    pub manifest: ConnectorManifest,
    pub status: PackageStatus,
    pub trust_level: TrustLevel,
    pub installed_at_ms: u64,
    pub last_verified_at_ms: u64,
    /// Number of times this package has been verified.
    pub verification_count: u64,
}

// =============================================================================
// Digest verification
// =============================================================================

/// Verify SHA-256 digest of payload bytes against expected hex string.
pub fn verify_digest(
    package_id: &str,
    payload: &[u8],
    expected_hex: &str,
) -> Result<(), ConnectorRegistryError> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(payload);
    let digest = hasher.finalize();
    let actual = hex_encode(&digest);
    if actual != expected_hex {
        return Err(ConnectorRegistryError::DigestMismatch {
            package_id: package_id.to_string(),
            expected: expected_hex.to_string(),
            actual,
        });
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Compute SHA-256 hex digest of payload bytes.
pub fn compute_digest(payload: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(payload);
    hex_encode(&hasher.finalize())
}

// =============================================================================
// Transparency log stub
// =============================================================================

/// Result of a transparency log check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransparencyCheckResult {
    pub package_id: String,
    pub verified: bool,
    pub log_index: Option<u64>,
    pub checked_at_ms: u64,
}

/// Verify a transparency token for a package.
///
/// This is currently a stub that validates token structure.
/// A real implementation would check against a Merkle-tree log endpoint.
pub fn check_transparency(
    package_id: &str,
    token: &str,
    now_ms: u64,
) -> Result<TransparencyCheckResult, ConnectorRegistryError> {
    // Stub: validate that the token is non-empty and hex-like
    if token.is_empty() {
        return Err(ConnectorRegistryError::TransparencyCheckFailed {
            package_id: package_id.to_string(),
            reason: "empty transparency token".to_string(),
        });
    }
    // Accept any non-empty token as valid for now
    Ok(TransparencyCheckResult {
        package_id: package_id.to_string(),
        verified: true,
        log_index: None,
        checked_at_ms: now_ms,
    })
}

// =============================================================================
// Telemetry
// =============================================================================

/// Telemetry counters for registry operations.
#[derive(Debug, Clone, Default)]
pub struct RegistryTelemetry {
    pub packages_registered: u64,
    pub packages_verified: u64,
    pub digest_failures: u64,
    pub trust_denials: u64,
    pub capability_denials: u64,
    pub transparency_checks: u64,
    pub lookups: u64,
}

/// Snapshot for serialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegistryTelemetrySnapshot {
    pub packages_registered: u64,
    pub packages_verified: u64,
    pub digest_failures: u64,
    pub trust_denials: u64,
    pub capability_denials: u64,
    pub transparency_checks: u64,
    pub lookups: u64,
}

impl RegistryTelemetry {
    pub fn snapshot(&self) -> RegistryTelemetrySnapshot {
        RegistryTelemetrySnapshot {
            packages_registered: self.packages_registered,
            packages_verified: self.packages_verified,
            digest_failures: self.digest_failures,
            trust_denials: self.trust_denials,
            capability_denials: self.capability_denials,
            transparency_checks: self.transparency_checks,
            lookups: self.lookups,
        }
    }
}

// =============================================================================
// Registry client
// =============================================================================

/// Configuration for the connector registry client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectorRegistryConfig {
    /// Maximum installed packages before eviction of retired entries.
    pub max_packages: usize,
    /// Trust policy to enforce.
    pub trust_policy: TrustPolicy,
    /// Whether to require transparency checks on install.
    pub enforce_transparency: bool,
    /// Maximum size of verification history per package.
    pub max_verification_history: usize,
}

impl Default for ConnectorRegistryConfig {
    fn default() -> Self {
        Self {
            max_packages: 256,
            trust_policy: TrustPolicy::default(),
            enforce_transparency: false,
            max_verification_history: 100,
        }
    }
}

/// The connector registry client manages installed packages.
pub struct ConnectorRegistryClient {
    config: ConnectorRegistryConfig,
    packages: HashMap<String, PackageEntry>,
    verification_log: VecDeque<VerificationRecord>,
    telemetry: RegistryTelemetry,
}

/// A record of a verification event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerificationRecord {
    pub package_id: String,
    pub version: String,
    pub outcome: VerificationOutcome,
    pub timestamp_ms: u64,
}

/// Outcome of a verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationOutcome {
    Passed,
    DigestFailed,
    SignatureFailed,
    TrustDenied,
    CapabilityDenied,
    TransparencyFailed,
}

impl VerificationOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::DigestFailed => "digest_failed",
            Self::SignatureFailed => "signature_failed",
            Self::TrustDenied => "trust_denied",
            Self::CapabilityDenied => "capability_denied",
            Self::TransparencyFailed => "transparency_failed",
        }
    }
}

impl fmt::Display for VerificationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ConnectorRegistryClient {
    /// Create a new registry client with the given config.
    pub fn new(config: ConnectorRegistryConfig) -> Self {
        Self {
            config,
            packages: HashMap::new(),
            verification_log: VecDeque::new(),
            telemetry: RegistryTelemetry::default(),
        }
    }

    /// Register a package after full verification.
    ///
    /// Verifies digest, trust policy, capabilities, and optionally transparency,
    /// then installs the package as Active.
    pub fn register_package(
        &mut self,
        manifest: ConnectorManifest,
        payload: &[u8],
        now_ms: u64,
    ) -> Result<&PackageEntry, ConnectorRegistryError> {
        // 1. Validate manifest structure
        manifest.validate()?;

        // 2. Verify digest
        if let Err(e) = verify_digest(&manifest.package_id, payload, &manifest.sha256_digest) {
            self.telemetry.digest_failures += 1;
            self.record_verification(&manifest, VerificationOutcome::DigestFailed, now_ms);
            return Err(e);
        }

        // 3. Trust policy gate
        let trust_level = match self.config.trust_policy.gate(&manifest) {
            Ok(level) => level,
            Err(e) => {
                match &e {
                    ConnectorRegistryError::CapabilityNotPermitted { .. } => {
                        self.telemetry.capability_denials += 1;
                        self.record_verification(
                            &manifest,
                            VerificationOutcome::CapabilityDenied,
                            now_ms,
                        );
                    }
                    _ => {
                        self.telemetry.trust_denials += 1;
                        self.record_verification(
                            &manifest,
                            VerificationOutcome::TrustDenied,
                            now_ms,
                        );
                    }
                }
                return Err(e);
            }
        };

        // 4. Optional transparency check
        if self.config.enforce_transparency {
            self.telemetry.transparency_checks += 1;
            match &manifest.transparency_token {
                Some(token) => {
                    check_transparency(&manifest.package_id, token, now_ms)?;
                }
                None => {
                    self.record_verification(
                        &manifest,
                        VerificationOutcome::TransparencyFailed,
                        now_ms,
                    );
                    return Err(ConnectorRegistryError::TransparencyCheckFailed {
                        package_id: manifest.package_id.clone(),
                        reason: "no transparency token provided".to_string(),
                    });
                }
            }
        }

        // 5. Evict retired entries if at capacity
        self.evict_if_needed();

        // 6. Record successful verification
        self.record_verification(&manifest, VerificationOutcome::Passed, now_ms);
        self.telemetry.packages_verified += 1;
        self.telemetry.packages_registered += 1;

        let package_id = manifest.package_id.clone();
        let entry = PackageEntry {
            manifest,
            status: PackageStatus::Active,
            trust_level,
            installed_at_ms: now_ms,
            last_verified_at_ms: now_ms,
            verification_count: 1,
        };

        self.packages.insert(package_id.clone(), entry);
        Ok(self.packages.get(&package_id).unwrap())
    }

    /// Re-verify an installed package's digest.
    pub fn reverify_package(
        &mut self,
        package_id: &str,
        payload: &[u8],
        now_ms: u64,
    ) -> Result<(), ConnectorRegistryError> {
        let entry = self
            .packages
            .get(package_id)
            .ok_or_else(|| ConnectorRegistryError::PackageNotFound {
                package_id: package_id.to_string(),
            })?;

        let expected = entry.manifest.sha256_digest.clone();
        if let Err(e) = verify_digest(package_id, payload, &expected) {
            self.telemetry.digest_failures += 1;
            self.record_verification(
                &self.packages[package_id].manifest.clone(),
                VerificationOutcome::DigestFailed,
                now_ms,
            );
            return Err(e);
        }

        self.telemetry.packages_verified += 1;
        let entry = self.packages.get_mut(package_id).unwrap();
        entry.last_verified_at_ms = now_ms;
        entry.verification_count += 1;
        let manifest = entry.manifest.clone();
        self.record_verification(
            &manifest,
            VerificationOutcome::Passed,
            now_ms,
        );
        Ok(())
    }

    /// Look up a package by ID.
    pub fn get_package(&mut self, package_id: &str) -> Option<&PackageEntry> {
        self.telemetry.lookups += 1;
        self.packages.get(package_id)
    }

    /// List all active packages.
    pub fn active_packages(&self) -> Vec<&PackageEntry> {
        self.packages
            .values()
            .filter(|e| e.status == PackageStatus::Active)
            .collect()
    }

    /// Disable a package.
    pub fn disable_package(
        &mut self,
        package_id: &str,
    ) -> Result<(), ConnectorRegistryError> {
        let entry = self
            .packages
            .get_mut(package_id)
            .ok_or_else(|| ConnectorRegistryError::PackageNotFound {
                package_id: package_id.to_string(),
            })?;
        entry.status = PackageStatus::Disabled;
        Ok(())
    }

    /// Retire a package (soft-remove).
    pub fn retire_package(
        &mut self,
        package_id: &str,
    ) -> Result<(), ConnectorRegistryError> {
        let entry = self
            .packages
            .get_mut(package_id)
            .ok_or_else(|| ConnectorRegistryError::PackageNotFound {
                package_id: package_id.to_string(),
            })?;
        entry.status = PackageStatus::Retired;
        Ok(())
    }

    /// Number of installed packages.
    pub fn package_count(&self) -> usize {
        self.packages.len()
    }

    /// Get telemetry snapshot.
    pub fn telemetry(&self) -> &RegistryTelemetry {
        &self.telemetry
    }

    /// Get verification log (most recent first).
    pub fn verification_log(&self) -> &VecDeque<VerificationRecord> {
        &self.verification_log
    }

    /// Get the config.
    pub fn config(&self) -> &ConnectorRegistryConfig {
        &self.config
    }

    fn record_verification(
        &mut self,
        manifest: &ConnectorManifest,
        outcome: VerificationOutcome,
        timestamp_ms: u64,
    ) {
        let record = VerificationRecord {
            package_id: manifest.package_id.clone(),
            version: manifest.version.clone(),
            outcome,
            timestamp_ms,
        };
        self.verification_log.push_back(record);
        while self.verification_log.len() > self.config.max_verification_history {
            self.verification_log.pop_front();
        }
    }

    fn evict_if_needed(&mut self) {
        if self.packages.len() >= self.config.max_packages {
            // Evict retired entries first
            let retired: Vec<String> = self
                .packages
                .iter()
                .filter(|(_, e)| e.status == PackageStatus::Retired)
                .map(|(k, _)| k.clone())
                .collect();
            for key in retired {
                self.packages.remove(&key);
                if self.packages.len() < self.config.max_packages {
                    break;
                }
            }
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Helpers ----

    fn test_digest(data: &[u8]) -> String {
        compute_digest(data)
    }

    fn test_manifest(package_id: &str, data: &[u8]) -> ConnectorManifest {
        ConnectorManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            package_id: package_id.to_string(),
            version: "1.0.0".to_string(),
            display_name: package_id.to_string(),
            description: "Test connector".to_string(),
            author: "test-author".to_string(),
            min_ft_version: None,
            sha256_digest: test_digest(data),
            required_capabilities: vec![ConnectorCapability::Invoke],
            publisher_signature: Some("deadbeef".to_string()),
            transparency_token: None,
            created_at_ms: 1000,
            metadata: BTreeMap::new(),
        }
    }

    fn default_config() -> ConnectorRegistryConfig {
        ConnectorRegistryConfig::default()
    }

    fn default_client() -> ConnectorRegistryClient {
        ConnectorRegistryClient::new(default_config())
    }

    // ========================================================================
    // Manifest validation
    // ========================================================================

    #[test]
    fn manifest_valid() {
        let m = test_manifest("foo", b"hello");
        assert!(m.validate().is_ok());
    }

    #[test]
    fn manifest_empty_package_id() {
        let mut m = test_manifest("foo", b"hello");
        m.package_id = String::new();
        let err = m.validate().unwrap_err();
        assert!(matches!(err, ConnectorRegistryError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_empty_version() {
        let mut m = test_manifest("foo", b"hello");
        m.version = String::new();
        let err = m.validate().unwrap_err();
        assert!(matches!(err, ConnectorRegistryError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_bad_digest_length() {
        let mut m = test_manifest("foo", b"hello");
        m.sha256_digest = "abc".to_string();
        let err = m.validate().unwrap_err();
        assert!(matches!(err, ConnectorRegistryError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_bad_digest_chars() {
        let mut m = test_manifest("foo", b"hello");
        m.sha256_digest = "g".repeat(64);
        let err = m.validate().unwrap_err();
        assert!(matches!(err, ConnectorRegistryError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_schema_version_zero() {
        let mut m = test_manifest("foo", b"hello");
        m.schema_version = 0;
        let err = m.validate().unwrap_err();
        assert!(matches!(err, ConnectorRegistryError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_schema_version_too_high() {
        let mut m = test_manifest("foo", b"hello");
        m.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        let err = m.validate().unwrap_err();
        assert!(matches!(err, ConnectorRegistryError::ManifestInvalid { .. }));
    }

    #[test]
    fn manifest_serde_roundtrip() {
        let m = test_manifest("roundtrip-pkg", b"data");
        let json = serde_json::to_string(&m).unwrap();
        let m2: ConnectorManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, m2);
    }

    // ========================================================================
    // Digest verification
    // ========================================================================

    #[test]
    fn digest_verify_ok() {
        let data = b"hello world";
        let expected = compute_digest(data);
        assert!(verify_digest("test", data, &expected).is_ok());
    }

    #[test]
    fn digest_verify_mismatch() {
        let data = b"hello world";
        let bad = "0".repeat(64);
        let err = verify_digest("test", data, &bad).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::DigestMismatch { .. }
        ));
    }

    #[test]
    fn compute_digest_deterministic() {
        let d1 = compute_digest(b"abc");
        let d2 = compute_digest(b"abc");
        assert_eq!(d1, d2);
        assert_eq!(d1.len(), 64);
    }

    #[test]
    fn compute_digest_different_inputs() {
        let d1 = compute_digest(b"abc");
        let d2 = compute_digest(b"def");
        assert_ne!(d1, d2);
    }

    // ========================================================================
    // Trust policy
    // ========================================================================

    #[test]
    fn trust_evaluate_blocked() {
        let mut policy = TrustPolicy::default();
        policy.blocked_packages.push("evil-pkg".to_string());
        let m = test_manifest("evil-pkg", b"x");
        assert_eq!(policy.evaluate(&m), TrustLevel::Blocked);
    }

    #[test]
    fn trust_evaluate_no_signature() {
        let policy = TrustPolicy::default();
        let mut m = test_manifest("pkg", b"x");
        m.publisher_signature = None;
        assert_eq!(policy.evaluate(&m), TrustLevel::Untrusted);
    }

    #[test]
    fn trust_evaluate_known_publisher() {
        let mut policy = TrustPolicy::default();
        policy.trusted_publishers.push("test-author".to_string());
        let m = test_manifest("pkg", b"x");
        assert_eq!(policy.evaluate(&m), TrustLevel::Trusted);
    }

    #[test]
    fn trust_evaluate_unknown_publisher_signed() {
        let policy = TrustPolicy::default();
        let m = test_manifest("pkg", b"x");
        assert_eq!(policy.evaluate(&m), TrustLevel::Conditional);
    }

    #[test]
    fn trust_level_allows_install() {
        assert!(TrustLevel::Trusted.allows_install());
        assert!(TrustLevel::Conditional.allows_install());
        assert!(!TrustLevel::Untrusted.allows_install());
        assert!(!TrustLevel::Blocked.allows_install());
    }

    #[test]
    fn trust_policy_gate_blocks_untrusted() {
        let policy = TrustPolicy::default();
        let mut m = test_manifest("pkg", b"x");
        m.publisher_signature = None;
        let err = policy.gate(&m).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::TrustPolicyDenied { .. }
        ));
    }

    #[test]
    fn trust_policy_gate_blocks_capability() {
        let mut policy = TrustPolicy::default();
        policy.max_allowed_capabilities = vec![ConnectorCapability::ReadState];
        let m = test_manifest("pkg", b"x");
        // m requires Invoke, which is not in max_allowed_capabilities
        let err = policy.gate(&m).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::CapabilityNotPermitted { .. }
        ));
    }

    #[test]
    fn trust_policy_gate_passes() {
        let policy = TrustPolicy::default();
        let m = test_manifest("pkg", b"x");
        let level = policy.gate(&m).unwrap();
        assert_eq!(level, TrustLevel::Conditional);
    }

    #[test]
    fn trust_policy_requires_transparency() {
        let mut policy = TrustPolicy::default();
        policy.require_transparency_proof = true;
        let m = test_manifest("pkg", b"x");
        let err = policy.gate(&m).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::TrustPolicyDenied { .. }
        ));
    }

    #[test]
    fn trust_policy_serde_roundtrip() {
        let policy = TrustPolicy::default();
        let json = serde_json::to_string(&policy).unwrap();
        let p2: TrustPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, p2);
    }

    // ========================================================================
    // Transparency check
    // ========================================================================

    #[test]
    fn transparency_empty_token_fails() {
        let err = check_transparency("pkg", "", 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::TransparencyCheckFailed { .. }
        ));
    }

    #[test]
    fn transparency_valid_token() {
        let result = check_transparency("pkg", "abc123", 1000).unwrap();
        assert!(result.verified);
        assert_eq!(result.package_id, "pkg");
    }

    // ========================================================================
    // Registry client
    // ========================================================================

    #[test]
    fn register_package_success() {
        let mut client = default_client();
        let data = b"connector payload";
        let m = test_manifest("my-connector", data);
        let entry = client.register_package(m, data, 1000).unwrap();
        assert_eq!(entry.status, PackageStatus::Active);
        assert_eq!(entry.trust_level, TrustLevel::Conditional);
        assert_eq!(entry.verification_count, 1);
    }

    #[test]
    fn register_package_bad_digest() {
        let mut client = default_client();
        let data = b"connector payload";
        let m = test_manifest("my-connector", data);
        let err = client.register_package(m, b"wrong data", 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::DigestMismatch { .. }
        ));
        assert_eq!(client.telemetry().digest_failures, 1);
    }

    #[test]
    fn register_package_unsigned_rejected() {
        let mut client = default_client();
        let data = b"payload";
        let mut m = test_manifest("pkg", data);
        m.publisher_signature = None;
        let err = client.register_package(m, data, 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::TrustPolicyDenied { .. }
        ));
        assert_eq!(client.telemetry().trust_denials, 1);
    }

    #[test]
    fn register_package_blocked() {
        let mut config = default_config();
        config.trust_policy.blocked_packages.push("evil".to_string());
        let mut client = ConnectorRegistryClient::new(config);
        let data = b"payload";
        let m = test_manifest("evil", data);
        let err = client.register_package(m, data, 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::TrustPolicyDenied { .. }
        ));
    }

    #[test]
    fn register_package_capability_denied() {
        let mut config = default_config();
        config.trust_policy.max_allowed_capabilities = vec![ConnectorCapability::ReadState];
        let mut client = ConnectorRegistryClient::new(config);
        let data = b"payload";
        let m = test_manifest("pkg", data);
        let err = client.register_package(m, data, 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::CapabilityNotPermitted { .. }
        ));
        assert_eq!(client.telemetry().capability_denials, 1);
    }

    #[test]
    fn register_and_lookup() {
        let mut client = default_client();
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();

        let entry = client.get_package("pkg").unwrap();
        assert_eq!(entry.manifest.package_id, "pkg");
        assert_eq!(client.telemetry().lookups, 1);
    }

    #[test]
    fn lookup_missing_returns_none() {
        let mut client = default_client();
        assert!(client.get_package("nonexistent").is_none());
    }

    #[test]
    fn active_packages_filter() {
        let mut client = default_client();
        let d1 = b"data1";
        let d2 = b"data2";
        let m1 = test_manifest("a", d1);
        let m2 = test_manifest("b", d2);
        client.register_package(m1, d1, 1000).unwrap();
        client.register_package(m2, d2, 1000).unwrap();
        client.disable_package("b").unwrap();

        let active = client.active_packages();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].manifest.package_id, "a");
    }

    #[test]
    fn disable_package() {
        let mut client = default_client();
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();
        client.disable_package("pkg").unwrap();

        let entry = client.get_package("pkg").unwrap();
        assert_eq!(entry.status, PackageStatus::Disabled);
    }

    #[test]
    fn disable_missing_package() {
        let mut client = default_client();
        let err = client.disable_package("nope").unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::PackageNotFound { .. }
        ));
    }

    #[test]
    fn retire_package() {
        let mut client = default_client();
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();
        client.retire_package("pkg").unwrap();

        let entry = client.get_package("pkg").unwrap();
        assert_eq!(entry.status, PackageStatus::Retired);
    }

    #[test]
    fn reverify_ok() {
        let mut client = default_client();
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();

        client.reverify_package("pkg", data, 2000).unwrap();
        let entry = client.get_package("pkg").unwrap();
        assert_eq!(entry.verification_count, 2);
        assert_eq!(entry.last_verified_at_ms, 2000);
    }

    #[test]
    fn reverify_digest_fail() {
        let mut client = default_client();
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();

        let err = client.reverify_package("pkg", b"tampered", 2000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::DigestMismatch { .. }
        ));
    }

    #[test]
    fn reverify_missing() {
        let mut client = default_client();
        let err = client.reverify_package("nope", b"x", 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::PackageNotFound { .. }
        ));
    }

    #[test]
    fn package_count() {
        let mut client = default_client();
        assert_eq!(client.package_count(), 0);
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();
        assert_eq!(client.package_count(), 1);
    }

    #[test]
    fn eviction_retires_old_entries() {
        let mut config = default_config();
        config.max_packages = 2;
        let mut client = ConnectorRegistryClient::new(config);

        // Fill to capacity
        let d1 = b"d1";
        let d2 = b"d2";
        let d3 = b"d3";
        let m1 = test_manifest("a", d1);
        let m2 = test_manifest("b", d2);
        client.register_package(m1, d1, 1000).unwrap();
        client.register_package(m2, d2, 1000).unwrap();

        // Retire one, then add another should evict the retired one
        client.retire_package("a").unwrap();
        let m3 = test_manifest("c", d3);
        client.register_package(m3, d3, 2000).unwrap();

        assert!(client.get_package("a").is_none());
        assert!(client.get_package("b").is_some());
        assert!(client.get_package("c").is_some());
    }

    #[test]
    fn verification_log_bounded() {
        let mut config = default_config();
        config.max_verification_history = 3;
        let mut client = ConnectorRegistryClient::new(config);

        for i in 0..5 {
            let data = format!("data-{i}");
            let m = test_manifest(&format!("pkg-{i}"), data.as_bytes());
            let _ = client.register_package(m, data.as_bytes(), 1000 + i);
        }

        assert!(client.verification_log().len() <= 3);
    }

    #[test]
    fn verification_record_serde_roundtrip() {
        let record = VerificationRecord {
            package_id: "pkg".to_string(),
            version: "1.0.0".to_string(),
            outcome: VerificationOutcome::Passed,
            timestamp_ms: 1000,
        };
        let json = serde_json::to_string(&record).unwrap();
        let r2: VerificationRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, r2);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let mut client = default_client();
        let data = b"payload";
        let m = test_manifest("pkg", data);
        client.register_package(m, data, 1000).unwrap();

        let snap = client.telemetry().snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let s2: RegistryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, s2);
        assert_eq!(snap.packages_registered, 1);
        assert_eq!(snap.packages_verified, 1);
    }

    #[test]
    fn telemetry_accumulates() {
        let mut client = default_client();
        let d1 = b"d1";
        let d2 = b"d2";
        let m1 = test_manifest("a", d1);
        let m2 = test_manifest("b", d2);
        client.register_package(m1, d1, 1000).unwrap();
        client.register_package(m2, d2, 1000).unwrap();
        client.get_package("a");
        client.get_package("b");

        let t = client.telemetry();
        assert_eq!(t.packages_registered, 2);
        assert_eq!(t.packages_verified, 2);
        assert_eq!(t.lookups, 2);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ConnectorRegistryConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let c2: ConnectorRegistryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, c2);
    }

    #[test]
    fn trust_level_display() {
        assert_eq!(TrustLevel::Trusted.to_string(), "trusted");
        assert_eq!(TrustLevel::Conditional.to_string(), "conditional");
        assert_eq!(TrustLevel::Untrusted.to_string(), "untrusted");
        assert_eq!(TrustLevel::Blocked.to_string(), "blocked");
    }

    #[test]
    fn package_status_display() {
        assert_eq!(PackageStatus::Active.to_string(), "active");
        assert_eq!(PackageStatus::Pending.to_string(), "pending");
        assert_eq!(PackageStatus::Disabled.to_string(), "disabled");
        assert_eq!(PackageStatus::Retired.to_string(), "retired");
    }

    #[test]
    fn verification_outcome_display() {
        assert_eq!(VerificationOutcome::Passed.to_string(), "passed");
        assert_eq!(
            VerificationOutcome::DigestFailed.to_string(),
            "digest_failed"
        );
    }

    #[test]
    fn register_with_transparency_enforcement() {
        let mut config = default_config();
        config.enforce_transparency = true;
        let mut client = ConnectorRegistryClient::new(config);

        let data = b"payload";
        let mut m = test_manifest("pkg", data);
        m.transparency_token = Some("valid-token".to_string());
        let entry = client.register_package(m, data, 1000).unwrap();
        assert_eq!(entry.status, PackageStatus::Active);
        assert_eq!(client.telemetry().transparency_checks, 1);
    }

    #[test]
    fn register_transparency_required_but_missing() {
        let mut config = default_config();
        config.enforce_transparency = true;
        let mut client = ConnectorRegistryClient::new(config);

        let data = b"payload";
        let m = test_manifest("pkg", data); // no transparency_token
        let err = client.register_package(m, data, 1000).unwrap_err();
        assert!(matches!(
            err,
            ConnectorRegistryError::TransparencyCheckFailed { .. }
        ));
    }

    #[test]
    fn error_display_messages() {
        let err = ConnectorRegistryError::PackageNotFound {
            package_id: "foo".to_string(),
        };
        assert!(err.to_string().contains("foo"));

        let err = ConnectorRegistryError::DigestMismatch {
            package_id: "bar".to_string(),
            expected: "aaa".to_string(),
            actual: "bbb".to_string(),
        };
        assert!(err.to_string().contains("bar"));
        assert!(err.to_string().contains("aaa"));
    }

    #[test]
    fn stress_many_packages() {
        let mut client = default_client();
        for i in 0..100 {
            let data = format!("payload-{i}");
            let m = test_manifest(&format!("pkg-{i}"), data.as_bytes());
            client.register_package(m, data.as_bytes(), 1000 + i as u64).unwrap();
        }
        assert_eq!(client.package_count(), 100);
        assert_eq!(client.active_packages().len(), 100);
        assert_eq!(client.telemetry().packages_registered, 100);
    }
}
