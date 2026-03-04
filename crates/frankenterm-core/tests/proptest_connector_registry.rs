//! Property-based tests for connector_registry module.
//!
//! Coverage targets:
//! - TrustPolicy.evaluate() determinism and trust-level ordering
//! - TrustPolicy.gate() invariants (allows_install ↔ pass, capability checks)
//! - ConnectorManifest.validate() boundary cases
//! - Digest verification determinism and tamper detection
//! - Serde roundtrip for public types
//! - ConnectorRegistryClient registration and telemetry consistency
//!
//! ft-3681t.5.10 quality support slice (connector registry).

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::connector_host_runtime::ConnectorCapability;
use frankenterm_core::connector_registry::{
    compute_digest, verify_digest, ConnectorManifest, ConnectorRegistryClient,
    ConnectorRegistryConfig, PackageStatus, RegistryTelemetrySnapshot,
    TrustLevel, TrustPolicy, VerificationOutcome, VerificationRecord,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_package_id() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9\\-]{1,30}"
}

fn arb_semver() -> impl Strategy<Value = String> {
    (0u32..100, 0u32..100, 0u32..100).prop_map(|(a, b, c)| format!("{a}.{b}.{c}"))
}

fn arb_sha256() -> impl Strategy<Value = String> {
    "[0-9a-f]{64}"
}

fn arb_capability() -> impl Strategy<Value = ConnectorCapability> {
    prop_oneof![
        Just(ConnectorCapability::Invoke),
        Just(ConnectorCapability::ReadState),
        Just(ConnectorCapability::StreamEvents),
        Just(ConnectorCapability::FilesystemRead),
        Just(ConnectorCapability::FilesystemWrite),
        Just(ConnectorCapability::NetworkEgress),
        Just(ConnectorCapability::SecretBroker),
        Just(ConnectorCapability::ProcessExec),
    ]
}

fn arb_capabilities() -> impl Strategy<Value = Vec<ConnectorCapability>> {
    proptest::collection::vec(arb_capability(), 0..5).prop_map(|mut caps| {
        caps.sort_by_key(|c| format!("{c:?}"));
        caps.dedup();
        caps
    })
}

fn arb_trust_level() -> impl Strategy<Value = TrustLevel> {
    prop_oneof![
        Just(TrustLevel::Blocked),
        Just(TrustLevel::Untrusted),
        Just(TrustLevel::Conditional),
        Just(TrustLevel::Trusted),
    ]
}

fn arb_valid_manifest() -> impl Strategy<Value = ConnectorManifest> {
    (
        arb_package_id(),
        arb_semver(),
        arb_sha256(),
        arb_capabilities(),
        proptest::option::of("[a-f0-9]{16,32}"),
    )
        .prop_map(
            |(package_id, version, sha256_digest, required_capabilities, publisher_signature)| {
                ConnectorManifest {
                    schema_version: 1,
                    package_id: package_id.clone(),
                    version,
                    display_name: package_id,
                    description: String::new(),
                    author: "test-author".to_string(),
                    min_ft_version: None,
                    sha256_digest,
                    required_capabilities,
                    publisher_signature,
                    transparency_token: None,
                    created_at_ms: 0,
                    metadata: BTreeMap::new(),
                }
            },
        )
}

fn arb_package_status() -> impl Strategy<Value = PackageStatus> {
    prop_oneof![
        Just(PackageStatus::Active),
        Just(PackageStatus::Pending),
        Just(PackageStatus::Disabled),
        Just(PackageStatus::Retired),
    ]
}

fn arb_verification_outcome() -> impl Strategy<Value = VerificationOutcome> {
    prop_oneof![
        Just(VerificationOutcome::Passed),
        Just(VerificationOutcome::DigestFailed),
        Just(VerificationOutcome::SignatureFailed),
        Just(VerificationOutcome::TrustDenied),
        Just(VerificationOutcome::CapabilityDenied),
        Just(VerificationOutcome::TransparencyFailed),
    ]
}

// ---------------------------------------------------------------------------
// TrustLevel property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// allows_install is true only for Trusted and Conditional.
    #[test]
    fn trust_level_allows_install_invariant(level in arb_trust_level()) {
        let expected = matches!(level, TrustLevel::Trusted | TrustLevel::Conditional);
        prop_assert_eq!(level.allows_install(), expected);
    }

    /// Display/as_str roundtrip: as_str always produces a non-empty stable string.
    #[test]
    fn trust_level_as_str_nonempty(level in arb_trust_level()) {
        let s = level.as_str();
        prop_assert!(!s.is_empty());
        prop_assert_eq!(format!("{level}"), s);
    }
}

// ---------------------------------------------------------------------------
// TrustPolicy.evaluate() property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Blocked packages always evaluate to Blocked regardless of signature.
    #[test]
    fn evaluate_blocked_always_blocked(
        manifest in arb_valid_manifest(),
    ) {
        let policy = TrustPolicy {
            blocked_packages: vec![manifest.package_id.clone()],
            ..Default::default()
        };
        prop_assert_eq!(policy.evaluate(&manifest), TrustLevel::Blocked);
    }

    /// Unsigned manifests (no publisher_signature) evaluate to Untrusted.
    #[test]
    fn evaluate_unsigned_is_untrusted(mut manifest in arb_valid_manifest()) {
        manifest.publisher_signature = None;
        let policy = TrustPolicy {
            blocked_packages: Vec::new(),
            ..Default::default()
        };
        prop_assert_eq!(policy.evaluate(&manifest), TrustLevel::Untrusted);
    }

    /// Signed manifests from known publishers evaluate to Trusted.
    #[test]
    fn evaluate_known_publisher_is_trusted(mut manifest in arb_valid_manifest()) {
        manifest.publisher_signature = Some("sig".to_string());
        let policy = TrustPolicy {
            trusted_publishers: vec![manifest.author.clone()],
            blocked_packages: Vec::new(),
            ..Default::default()
        };
        prop_assert_eq!(policy.evaluate(&manifest), TrustLevel::Trusted);
    }

    /// Signed manifests from unknown publishers evaluate to Conditional.
    #[test]
    fn evaluate_unknown_publisher_is_conditional(mut manifest in arb_valid_manifest()) {
        manifest.publisher_signature = Some("sig".to_string());
        manifest.author = "unknown-author".to_string();
        let policy = TrustPolicy {
            trusted_publishers: vec!["other-publisher".to_string()],
            blocked_packages: Vec::new(),
            ..Default::default()
        };
        prop_assert_eq!(policy.evaluate(&manifest), TrustLevel::Conditional);
    }

    /// evaluate() is deterministic for the same (policy, manifest) pair.
    #[test]
    fn evaluate_deterministic(manifest in arb_valid_manifest()) {
        let policy = TrustPolicy::default();
        let r1 = policy.evaluate(&manifest);
        let r2 = policy.evaluate(&manifest);
        prop_assert_eq!(r1, r2);
    }
}

// ---------------------------------------------------------------------------
// TrustPolicy.gate() property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// gate() passes iff evaluate().allows_install() && capabilities are covered.
    #[test]
    fn gate_passes_iff_allows_install_and_capabilities(manifest in arb_valid_manifest()) {
        let policy = TrustPolicy {
            require_signature: false,
            require_transparency_proof: false,
            max_allowed_capabilities: vec![
                ConnectorCapability::Invoke,
                ConnectorCapability::ReadState,
                ConnectorCapability::StreamEvents,
                ConnectorCapability::FilesystemRead,
                ConnectorCapability::FilesystemWrite,
                ConnectorCapability::NetworkEgress,
                ConnectorCapability::SecretBroker,
                ConnectorCapability::ProcessExec,
            ],
            blocked_packages: Vec::new(),
            trusted_publishers: Vec::new(),
            min_install_level: TrustLevel::Conditional,
        };
        let level = policy.evaluate(&manifest);
        let result = policy.gate(&manifest);
        if level.allows_install() {
            prop_assert!(result.is_ok(), "gate should pass when allows_install is true");
        } else {
            prop_assert!(result.is_err(), "gate should fail when allows_install is false");
        }
    }

    /// gate() rejects capabilities not in max_allowed.
    #[test]
    fn gate_rejects_excess_capabilities(mut manifest in arb_valid_manifest()) {
        manifest.publisher_signature = Some("sig".to_string());
        manifest.required_capabilities = vec![ConnectorCapability::ProcessExec];
        let policy = TrustPolicy {
            require_signature: false,
            require_transparency_proof: false,
            max_allowed_capabilities: vec![ConnectorCapability::Invoke],
            blocked_packages: Vec::new(),
            trusted_publishers: Vec::new(),
            min_install_level: TrustLevel::Conditional,
        };
        let result = policy.gate(&manifest);
        prop_assert!(result.is_err(), "gate should reject uncovered capabilities");
    }
}

// ---------------------------------------------------------------------------
// ConnectorManifest.validate() property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Valid manifests always pass validation.
    #[test]
    fn validate_valid_manifest_passes(manifest in arb_valid_manifest()) {
        prop_assert!(manifest.validate().is_ok());
    }

    /// Empty package_id fails validation.
    #[test]
    fn validate_empty_package_id_fails(mut manifest in arb_valid_manifest()) {
        manifest.package_id = String::new();
        prop_assert!(manifest.validate().is_err());
    }

    /// Empty version fails validation.
    #[test]
    fn validate_empty_version_fails(mut manifest in arb_valid_manifest()) {
        manifest.version = String::new();
        prop_assert!(manifest.validate().is_err());
    }

    /// Invalid digest (wrong length) fails validation.
    #[test]
    fn validate_bad_digest_fails(mut manifest in arb_valid_manifest()) {
        manifest.sha256_digest = "tooshort".to_string();
        prop_assert!(manifest.validate().is_err());
    }

    /// Schema version 0 fails validation.
    #[test]
    fn validate_schema_v0_fails(mut manifest in arb_valid_manifest()) {
        manifest.schema_version = 0;
        prop_assert!(manifest.validate().is_err());
    }

    /// Schema version > 1 fails validation.
    #[test]
    fn validate_future_schema_fails(mut manifest in arb_valid_manifest()) {
        manifest.schema_version = 99;
        prop_assert!(manifest.validate().is_err());
    }
}

// ---------------------------------------------------------------------------
// Digest verification property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// compute_digest is deterministic.
    #[test]
    fn digest_deterministic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let d1 = compute_digest(&data);
        let d2 = compute_digest(&data);
        prop_assert_eq!(d1, d2);
    }

    /// compute_digest always produces 64 hex chars.
    #[test]
    fn digest_always_64_hex(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        let digest = compute_digest(&data);
        prop_assert_eq!(digest.len(), 64);
        prop_assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// verify_digest(id, data, compute_digest(data)) always succeeds.
    #[test]
    fn verify_digest_self_consistent(
        id in arb_package_id(),
        data in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        let expected = compute_digest(&data);
        prop_assert!(verify_digest(&id, &data, &expected).is_ok());
    }

    /// Tampered data fails verify_digest.
    #[test]
    fn verify_digest_detects_tampering(
        id in arb_package_id(),
        data in proptest::collection::vec(any::<u8>(), 2..256),
    ) {
        let expected = compute_digest(&data);
        let mut tampered = data.clone();
        tampered[0] = tampered[0].wrapping_add(1);
        if tampered != data {
            let result = verify_digest(&id, &tampered, &expected);
            prop_assert!(result.is_err(), "tampered data should fail verification");
        }
    }
}

// ---------------------------------------------------------------------------
// Serde roundtrip tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ConnectorManifest serde roundtrip is lossless.
    #[test]
    fn manifest_serde_roundtrip(manifest in arb_valid_manifest()) {
        let json = serde_json::to_string(&manifest).unwrap();
        let decoded: ConnectorManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(manifest, decoded);
    }

    /// TrustPolicy serde roundtrip is lossless.
    #[test]
    fn trust_policy_serde_roundtrip(
        level in arb_trust_level(),
        caps in arb_capabilities(),
        require_sig in proptest::bool::ANY,
        require_transparency in proptest::bool::ANY,
    ) {
        let policy = TrustPolicy {
            min_install_level: level,
            require_signature: require_sig,
            require_transparency_proof: require_transparency,
            max_allowed_capabilities: caps,
            trusted_publishers: vec!["pub1".to_string()],
            blocked_packages: vec!["blocked1".to_string()],
        };
        let json = serde_json::to_string(&policy).unwrap();
        let decoded: TrustPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(policy, decoded);
    }

    /// TrustLevel serde roundtrip is lossless.
    #[test]
    fn trust_level_serde_roundtrip(level in arb_trust_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let decoded: TrustLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(level, decoded);
    }

    /// PackageStatus serde roundtrip.
    #[test]
    fn package_status_serde_roundtrip(status in arb_package_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let decoded: PackageStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, decoded);
    }

    /// VerificationOutcome serde roundtrip.
    #[test]
    fn verification_outcome_serde_roundtrip(outcome in arb_verification_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let decoded: VerificationOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, decoded);
    }

    /// VerificationRecord serde roundtrip.
    #[test]
    fn verification_record_serde_roundtrip(
        id in arb_package_id(),
        ver in arb_semver(),
        outcome in arb_verification_outcome(),
        ts in 0u64..1_000_000,
    ) {
        let record = VerificationRecord {
            package_id: id,
            version: ver,
            outcome,
            timestamp_ms: ts,
        };
        let json = serde_json::to_string(&record).unwrap();
        let decoded: VerificationRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(record, decoded);
    }

    /// RegistryTelemetrySnapshot serde roundtrip.
    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        registered in 0u64..1000,
        verified in 0u64..1000,
        failures in 0u64..1000,
    ) {
        let snap = RegistryTelemetrySnapshot {
            packages_registered: registered,
            packages_verified: verified,
            digest_failures: failures,
            trust_denials: 0,
            capability_denials: 0,
            transparency_checks: 0,
            lookups: 0,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let decoded: RegistryTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, decoded);
    }
}

// ---------------------------------------------------------------------------
// Registry client property tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Registration with correct digest succeeds for trusted packages.
    #[test]
    fn registry_register_with_correct_digest(
        id in arb_package_id(),
        ver in arb_semver(),
        payload in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        let digest = compute_digest(&payload);
        let manifest = ConnectorManifest {
            schema_version: 1,
            package_id: id.clone(),
            version: ver,
            display_name: id,
            description: String::new(),
            author: "known-pub".to_string(),
            min_ft_version: None,
            sha256_digest: digest,
            required_capabilities: vec![ConnectorCapability::Invoke],
            publisher_signature: Some("sig".to_string()),
            transparency_token: None,
            created_at_ms: 0,
            metadata: BTreeMap::new(),
        };
        let config = ConnectorRegistryConfig {
            max_packages: 256,
            trust_policy: TrustPolicy {
                trusted_publishers: vec!["known-pub".to_string()],
                require_signature: false,
                require_transparency_proof: false,
                max_allowed_capabilities: vec![ConnectorCapability::Invoke],
                blocked_packages: Vec::new(),
                min_install_level: TrustLevel::Conditional,
            },
            enforce_transparency: false,
            max_verification_history: 100,
        };
        let mut client = ConnectorRegistryClient::new(config);
        let result = client.register_package(manifest, &payload, 1000);
        prop_assert!(result.is_ok(), "registration should succeed with correct digest");
    }

    /// Registration with wrong digest always fails.
    #[test]
    fn registry_register_with_wrong_digest_fails(
        id in arb_package_id(),
        ver in arb_semver(),
        payload in proptest::collection::vec(any::<u8>(), 1..256),
    ) {
        let manifest = ConnectorManifest {
            schema_version: 1,
            package_id: id.clone(),
            version: ver,
            display_name: id,
            description: String::new(),
            author: "known-pub".to_string(),
            min_ft_version: None,
            sha256_digest: "0".repeat(64), // wrong digest
            required_capabilities: vec![],
            publisher_signature: Some("sig".to_string()),
            transparency_token: None,
            created_at_ms: 0,
            metadata: BTreeMap::new(),
        };
        let config = ConnectorRegistryConfig {
            max_packages: 256,
            trust_policy: TrustPolicy {
                trusted_publishers: vec!["known-pub".to_string()],
                require_signature: false,
                require_transparency_proof: false,
                max_allowed_capabilities: vec![],
                blocked_packages: Vec::new(),
                min_install_level: TrustLevel::Conditional,
            },
            enforce_transparency: false,
            max_verification_history: 100,
        };
        let mut client = ConnectorRegistryClient::new(config);
        let result = client.register_package(manifest, &payload, 1000);
        // Should fail unless the payload happens to hash to all zeros (astronomically unlikely)
        if compute_digest(&payload) != "0".repeat(64) {
            prop_assert!(result.is_err(), "wrong digest should fail");
        }
    }
}
