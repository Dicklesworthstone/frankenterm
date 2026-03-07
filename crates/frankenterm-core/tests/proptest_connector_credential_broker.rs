//! Property-based tests for the connector credential broker module.
//!
//! Tests cover scope subset logic, credential lifecycle (register, lease, rotate,
//! revoke), lease expiration, access rule enforcement, sensitivity ordering,
//! telemetry counter accuracy, audit log bounds, and serde roundtrips.

use proptest::prelude::*;

use frankenterm_core::connector_credential_broker::{
    ConnectorCredentialBroker, CredentialAccessRule, CredentialAuditType, CredentialBrokerError,
    CredentialBrokerTelemetry, CredentialBrokerTelemetrySnapshot, CredentialKind, CredentialLease,
    CredentialScope, CredentialSensitivity, CredentialState, LeaseState, ManagedCredential,
    ProviderStatus, SecretProviderConfig,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_sensitivity() -> impl Strategy<Value = CredentialSensitivity> {
    prop_oneof![
        Just(CredentialSensitivity::Low),
        Just(CredentialSensitivity::Medium),
        Just(CredentialSensitivity::High),
        Just(CredentialSensitivity::Critical),
    ]
}

fn arb_credential_kind() -> impl Strategy<Value = CredentialKind> {
    prop_oneof![
        Just(CredentialKind::ApiKey),
        Just(CredentialKind::OAuth2Client),
        Just(CredentialKind::OAuth2Token),
        Just(CredentialKind::TlsCertificate),
        Just(CredentialKind::SshKey),
        Just(CredentialKind::SymmetricKey),
        Just(CredentialKind::GenericSecret),
    ]
}

fn arb_provider_status() -> impl Strategy<Value = ProviderStatus> {
    prop_oneof![
        Just(ProviderStatus::Available),
        Just(ProviderStatus::Degraded),
        Just(ProviderStatus::Unavailable),
    ]
}

fn arb_provider_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("github".to_string()),
        Just("slack".to_string()),
        Just("aws".to_string()),
        Just("gcp".to_string()),
        Just("vault".to_string()),
        "[a-z]{3,10}".prop_map(|s| s),
    ]
}

fn arb_resource() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("*".to_string()),
        Just("repos/*".to_string()),
        Just("repos/specific".to_string()),
        Just("channels".to_string()),
        Just("buckets/data".to_string()),
        "[a-z]{2,8}(/[a-z]{2,8})?".prop_map(|s| s),
    ]
}

fn arb_operation() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("*".to_string()),
        Just("read".to_string()),
        Just("write".to_string()),
        Just("admin".to_string()),
        Just("delete".to_string()),
    ]
}

fn arb_operations() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_operation(), 1..=4)
}

fn arb_scope() -> impl Strategy<Value = CredentialScope> {
    (arb_provider_name(), arb_resource(), arb_operations())
        .prop_map(|(provider, resource, operations)| {
            CredentialScope::new(provider, resource, operations)
        })
}

fn arb_connector_id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("conn-alpha".to_string()),
        Just("conn-beta".to_string()),
        Just("conn-gamma".to_string()),
        "conn-[a-z]{3,8}".prop_map(|s| s),
    ]
}

fn arb_telemetry() -> impl Strategy<Value = CredentialBrokerTelemetry> {
    (
        0u64..=1000,
        0u64..=500,
        0u64..=500,
        0u64..=200,
        0u64..=100,
        0u64..=50,
        0u64..=200,
        0u64..=100,
        0u64..=50,
    )
        .prop_map(
            |(issued, expired, revoked, denied, rot_ok, rot_fail, reg, cred_rev, prov_reg)| {
                CredentialBrokerTelemetry {
                    leases_issued: issued,
                    leases_expired: expired,
                    leases_revoked: revoked,
                    access_denied: denied,
                    rotations_completed: rot_ok,
                    rotations_failed: rot_fail,
                    credentials_registered: reg,
                    credentials_revoked: cred_rev,
                    providers_registered: prov_reg,
                }
            },
        )
}

fn arb_telemetry_snapshot() -> impl Strategy<Value = CredentialBrokerTelemetrySnapshot> {
    (arb_telemetry(), 0u64..=u64::MAX, 0u32..=100, 0u32..=100, 0u32..=20).prop_map(
        |(counters, ts, leases, creds, provs)| CredentialBrokerTelemetrySnapshot {
            captured_at_ms: ts,
            counters,
            active_leases: leases,
            active_credentials: creds,
            active_providers: provs,
        },
    )
}

// Helper: set up a broker with a provider and credential ready for lease tests.
fn setup_broker_for_leasing(
    provider_max_sensitivity: CredentialSensitivity,
    cred_sensitivity: CredentialSensitivity,
    default_ttl_ms: u64,
) -> ConnectorCredentialBroker {
    let mut broker = ConnectorCredentialBroker::new();
    broker
        .register_provider(
            SecretProviderConfig {
                provider_id: "prov-1".to_string(),
                display_name: "Test Provider".to_string(),
                provider_type: "vault".to_string(),
                max_concurrent_leases: 100,
                default_lease_ttl_ms: default_ttl_ms,
                supports_rotation: true,
                max_sensitivity: provider_max_sensitivity,
            },
            1000,
        )
        .unwrap();
    broker
        .register_credential(
            ManagedCredential {
                credential_id: "cred-1".to_string(),
                provider_id: "prov-1".to_string(),
                kind: CredentialKind::ApiKey,
                sensitivity: cred_sensitivity,
                state: CredentialState::Active,
                permitted_scopes: vec![CredentialScope::new(
                    "github",
                    "*",
                    vec!["*".to_string()],
                )],
                version: 1,
                created_at_ms: 1000,
                expires_at_ms: 0,
                last_rotated_at_ms: 0,
                active_lease_count: 0,
            },
            1000,
        )
        .unwrap();
    broker.add_access_rule(CredentialAccessRule {
        rule_id: "allow-all".to_string(),
        connector_pattern: "*".to_string(),
        permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
        max_sensitivity: CredentialSensitivity::Critical,
        max_lease_ttl_ms: 0,
        max_concurrent_leases: 50,
    });
    broker
}

// =============================================================================
// Scope subset property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A scope is always a subset of itself.
    #[test]
    fn scope_is_subset_of_itself(
        provider in arb_provider_name(),
        resource in arb_resource(),
        ops in arb_operations(),
    ) {
        let scope = CredentialScope::new(provider, resource, ops);
        prop_assert!(scope.is_subset_of(&scope));
    }

    /// Wildcard resource+ops covers any narrower scope with same provider.
    #[test]
    fn wildcard_scope_covers_narrow(
        provider in arb_provider_name(),
        resource in arb_resource(),
        ops in arb_operations(),
    ) {
        let wide = CredentialScope::new(provider.clone(), "*", vec!["*".to_string()]);
        let narrow = CredentialScope::new(provider, resource, ops);
        prop_assert!(narrow.is_subset_of(&wide));
    }

    /// Different providers never match.
    #[test]
    fn different_providers_never_subset(
        resource in arb_resource(),
        ops in arb_operations(),
    ) {
        let a = CredentialScope::new("github", resource.clone(), ops.clone());
        let b = CredentialScope::new("slack", resource, ops);
        prop_assert!(!a.is_subset_of(&b));
        prop_assert!(!b.is_subset_of(&a));
    }

    /// Operations not in the parent scope cause subset check to fail.
    #[test]
    fn missing_operation_rejects_subset(
        provider in arb_provider_name(),
        resource in arb_resource(),
    ) {
        let parent = CredentialScope::new(provider.clone(), resource.clone(), vec!["read".to_string()]);
        let child = CredentialScope::new(provider, resource, vec!["read".to_string(), "write".to_string()]);
        prop_assert!(!child.is_subset_of(&parent));
    }
}

// =============================================================================
// Sensitivity ordering property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Sensitivity ordering is total and consistent.
    #[test]
    fn sensitivity_total_order(a in arb_sensitivity(), b in arb_sensitivity()) {
        // At least one of: a <= b, b <= a (total order)
        prop_assert!(a <= b || b <= a);
    }

    /// Sensitivity ordering is transitive.
    #[test]
    fn sensitivity_transitive(
        a in arb_sensitivity(),
        b in arb_sensitivity(),
        c in arb_sensitivity(),
    ) {
        if a <= b && b <= c {
            prop_assert!(a <= c);
        }
    }

    /// Display roundtrip: all variants produce non-empty strings.
    #[test]
    fn sensitivity_display_nonempty(s in arb_sensitivity()) {
        let display = s.to_string();
        prop_assert!(!display.is_empty());
    }
}

// =============================================================================
// CredentialKind display tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// All CredentialKind variants produce non-empty display strings.
    #[test]
    fn credential_kind_display_nonempty(kind in arb_credential_kind()) {
        let display = kind.to_string();
        prop_assert!(!display.is_empty());
        // All should be snake_case
        prop_assert!(display.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()));
    }
}

// =============================================================================
// Provider registration property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Registering N unique providers yields N providers in the broker.
    #[test]
    fn register_n_providers_counted(n in 1usize..=20) {
        let mut broker = ConnectorCredentialBroker::new();
        for i in 0..n {
            let config = SecretProviderConfig {
                provider_id: format!("p-{i}"),
                display_name: format!("Provider {i}"),
                provider_type: "env".to_string(),
                max_concurrent_leases: 10,
                default_lease_ttl_ms: 60_000,
                supports_rotation: false,
                max_sensitivity: CredentialSensitivity::Critical,
            };
            broker.register_provider(config, i as u64 * 100).unwrap();
        }
        prop_assert_eq!(broker.provider_ids().len(), n);
        prop_assert_eq!(broker.telemetry_snapshot(0).active_providers, n as u32);
    }

    /// Re-registering the same provider ID overwrites without error.
    #[test]
    fn reregister_provider_overwrites(
        status1 in arb_provider_status(),
        _status2 in arb_provider_status(),
    ) {
        let mut broker = ConnectorCredentialBroker::new();
        let config = SecretProviderConfig {
            provider_id: "p1".to_string(),
            display_name: "P1".to_string(),
            provider_type: "vault".to_string(),
            max_concurrent_leases: 10,
            default_lease_ttl_ms: 60_000,
            supports_rotation: true,
            max_sensitivity: CredentialSensitivity::Critical,
        };
        broker.register_provider(config, 100).unwrap();
        broker.update_provider_status("p1", status1, 200).unwrap();

        // Re-register resets to Available
        let config2 = SecretProviderConfig {
            provider_id: "p1".to_string(),
            display_name: "P1 v2".to_string(),
            provider_type: "keychain".to_string(),
            max_concurrent_leases: 20,
            default_lease_ttl_ms: 120_000,
            supports_rotation: false,
            max_sensitivity: CredentialSensitivity::High,
        };
        broker.register_provider(config2, 300).unwrap();
        let prov = broker.get_provider("p1").unwrap();
        prop_assert_eq!(prov.status, ProviderStatus::Available);
        prop_assert_eq!(&prov.config.display_name, "P1 v2");
        // Still only 1 provider
        prop_assert_eq!(broker.provider_ids().len(), 1);
    }

    /// Provider status updates are reflected in reads.
    #[test]
    fn provider_status_updates_stick(status in arb_provider_status()) {
        let mut broker = ConnectorCredentialBroker::new();
        let config = SecretProviderConfig {
            provider_id: "p1".to_string(),
            display_name: "P1".to_string(),
            provider_type: "vault".to_string(),
            max_concurrent_leases: 10,
            default_lease_ttl_ms: 60_000,
            supports_rotation: true,
            max_sensitivity: CredentialSensitivity::Critical,
        };
        broker.register_provider(config, 100).unwrap();
        broker.update_provider_status("p1", status, 200).unwrap();
        prop_assert_eq!(broker.get_provider("p1").unwrap().status, status);
    }
}

// =============================================================================
// Credential registration property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Credential with sensitivity exceeding provider max is rejected.
    #[test]
    fn credential_sensitivity_ceiling_enforced(
        prov_max in arb_sensitivity(),
        cred_sens in arb_sensitivity(),
    ) {
        let mut broker = ConnectorCredentialBroker::new();
        broker.register_provider(SecretProviderConfig {
            provider_id: "p1".to_string(),
            display_name: "P1".to_string(),
            provider_type: "vault".to_string(),
            max_concurrent_leases: 10,
            default_lease_ttl_ms: 60_000,
            supports_rotation: true,
            max_sensitivity: prov_max,
        }, 100).unwrap();

        let cred = ManagedCredential {
            credential_id: "c1".to_string(),
            provider_id: "p1".to_string(),
            kind: CredentialKind::ApiKey,
            sensitivity: cred_sens,
            state: CredentialState::Active,
            permitted_scopes: vec![],
            version: 1,
            created_at_ms: 100,
            expires_at_ms: 0,
            last_rotated_at_ms: 0,
            active_lease_count: 0,
        };
        let result = broker.register_credential(cred, 100);
        if cred_sens > prov_max {
            let is_err = result.is_err();
            prop_assert!(is_err, "should reject credential above provider sensitivity ceiling");
        } else {
            let is_ok = result.is_ok();
            prop_assert!(is_ok, "should accept credential within provider sensitivity ceiling");
        }
    }

    /// Credential for unknown provider is rejected.
    #[test]
    fn credential_unknown_provider_rejected(kind in arb_credential_kind()) {
        let mut broker = ConnectorCredentialBroker::new();
        let cred = ManagedCredential {
            credential_id: "c1".to_string(),
            provider_id: "nonexistent".to_string(),
            kind,
            sensitivity: CredentialSensitivity::Low,
            state: CredentialState::Active,
            permitted_scopes: vec![],
            version: 1,
            created_at_ms: 100,
            expires_at_ms: 0,
            last_rotated_at_ms: 0,
            active_lease_count: 0,
        };
        let err = broker.register_credential(cred, 100).unwrap_err();
        let is_provider_not_found = matches!(err, CredentialBrokerError::ProviderNotFound { .. });
        prop_assert!(is_provider_not_found);
    }
}

// =============================================================================
// Lease lifecycle property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Issuing a lease increments active_lease_count and telemetry.
    #[test]
    fn lease_issuance_increments_counters(
        n in 1usize..=10,
        ttl in 10_000u64..=3_600_000,
    ) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            ttl,
        );
        for i in 0..n {
            let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
            broker.request_lease(&format!("conn-{i}"), "cred-1", scope, 2000 + i as u64).unwrap();
        }
        let cred = broker.get_credential("cred-1").unwrap();
        prop_assert_eq!(cred.active_lease_count, n as u32);
        let snap = broker.telemetry_snapshot(5000);
        prop_assert_eq!(snap.counters.leases_issued, n as u64);
        prop_assert_eq!(snap.active_leases, n as u32);
    }

    /// Lease TTL uses provider default when no rule overrides.
    #[test]
    fn lease_ttl_from_provider_default(ttl in 10_000u64..=7_200_000) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            ttl,
        );
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
        let lease = broker.request_lease("conn-1", "cred-1", scope, 2000).unwrap();
        // The rule in setup_broker has max_lease_ttl_ms=0, so provider default is used
        prop_assert_eq!(lease.expires_at_ms, 2000u64.saturating_add(ttl));
    }

    /// Revoking a lease makes it non-active and decrements counters.
    #[test]
    fn revoke_lease_decrements_counters(n in 1usize..=8) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        let mut lease_ids = Vec::new();
        for i in 0..n {
            let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
            let lease = broker.request_lease(&format!("conn-{i}"), "cred-1", scope, 2000 + i as u64).unwrap();
            lease_ids.push(lease.lease_id);
        }
        // Revoke the first lease
        broker.revoke_lease(&lease_ids[0], 5000).unwrap();
        let cred = broker.get_credential("cred-1").unwrap();
        prop_assert_eq!(cred.active_lease_count, (n - 1) as u32);
        prop_assert_eq!(broker.telemetry_snapshot(5000).counters.leases_revoked, 1);
    }

    /// Revoking an already-revoked lease is a no-op (idempotent).
    #[test]
    fn revoke_lease_idempotent(_dummy in 0u8..1) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
        let lease = broker.request_lease("conn-1", "cred-1", scope, 2000).unwrap();
        broker.revoke_lease(&lease.lease_id, 3000).unwrap();
        // Second revoke should be a no-op
        broker.revoke_lease(&lease.lease_id, 4000).unwrap();
        prop_assert_eq!(broker.telemetry_snapshot(5000).counters.leases_revoked, 1);
    }

    /// Expired leases are not counted as active.
    #[test]
    fn expired_leases_not_active(
        n in 1usize..=5,
        ttl in 1000u64..=10_000,
    ) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            ttl,
        );
        for i in 0..n {
            let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
            broker.request_lease(&format!("conn-{i}"), "cred-1", scope, 2000).unwrap();
        }
        // Expire all leases
        let expired = broker.expire_leases(2000 + ttl + 1);
        prop_assert_eq!(expired.len(), n);
        prop_assert_eq!(broker.telemetry_snapshot(99999).active_leases, 0);
        prop_assert_eq!(broker.telemetry_snapshot(99999).counters.leases_expired, n as u64);
        // Credential active_lease_count should be 0
        let cred = broker.get_credential("cred-1").unwrap();
        prop_assert_eq!(cred.active_lease_count, 0);
    }
}

// =============================================================================
// Lease validity property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// A lease is valid strictly before expires_at_ms and if Active.
    #[test]
    fn lease_validity_before_expiry(
        issued in 0u64..=1_000_000,
        ttl in 1u64..=1_000_000,
        check_offset in 0u64..=2_000_000,
    ) {
        let expires = issued.saturating_add(ttl);
        let lease = CredentialLease {
            lease_id: "l1".to_string(),
            credential_id: "c1".to_string(),
            connector_id: "conn-1".to_string(),
            granted_scope: CredentialScope::new("github", "repos/x", vec!["read".to_string()]),
            state: LeaseState::Active,
            issued_at_ms: issued,
            expires_at_ms: expires,
            credential_version: 1,
        };
        let check_time = issued.saturating_add(check_offset);
        let expected_valid = check_time < expires;
        prop_assert_eq!(lease.is_valid_at(check_time), expected_valid);
    }

    /// Non-Active leases are never valid regardless of time.
    #[test]
    fn non_active_lease_never_valid(
        state in prop_oneof![Just(LeaseState::Expired), Just(LeaseState::Revoked)],
        check_time in 0u64..=1_000_000,
    ) {
        let lease = CredentialLease {
            lease_id: "l1".to_string(),
            credential_id: "c1".to_string(),
            connector_id: "conn-1".to_string(),
            granted_scope: CredentialScope::new("github", "repos/x", vec!["read".to_string()]),
            state,
            issued_at_ms: 0,
            expires_at_ms: u64::MAX,
            credential_version: 1,
        };
        prop_assert!(!lease.is_valid_at(check_time));
    }
}

// =============================================================================
// Access rule enforcement property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// With no access rules, no connector is authorized.
    #[test]
    fn no_rules_means_no_authorization(
        connector in arb_connector_id(),
        scope in arb_scope(),
        sens in arb_sensitivity(),
    ) {
        let broker = ConnectorCredentialBroker::new();
        prop_assert!(!broker.is_authorized(&connector, &scope, sens));
    }

    /// Wildcard connector pattern authorizes any connector.
    #[test]
    fn wildcard_connector_authorizes_all(
        connector in arb_connector_id(),
        sens in arb_sensitivity(),
    ) {
        let mut broker = ConnectorCredentialBroker::new();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "allow-all".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
            max_sensitivity: CredentialSensitivity::Critical,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 10,
        });
        let scope = CredentialScope::new("github", "repos/test", vec!["read".to_string()]);
        prop_assert!(broker.is_authorized(&connector, &scope, sens));
    }

    /// Specific connector pattern only matches that connector.
    #[test]
    fn specific_connector_pattern_matches_only_that(
        target in arb_connector_id(),
    ) {
        let mut broker = ConnectorCredentialBroker::new();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "specific".to_string(),
            connector_pattern: "conn-special".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
            max_sensitivity: CredentialSensitivity::Critical,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 10,
        });
        let scope = CredentialScope::new("github", "repos/test", vec!["read".to_string()]);
        let expected = target == "conn-special";
        prop_assert_eq!(broker.is_authorized(&target, &scope, CredentialSensitivity::Low), expected);
    }

    /// Sensitivity above rule max is denied.
    #[test]
    fn sensitivity_above_rule_max_denied(
        rule_max in arb_sensitivity(),
        request_sens in arb_sensitivity(),
    ) {
        let mut broker = ConnectorCredentialBroker::new();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "r1".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
            max_sensitivity: rule_max,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 10,
        });
        let scope = CredentialScope::new("github", "repos/test", vec!["read".to_string()]);
        let authorized = broker.is_authorized("conn-1", &scope, request_sens);
        if request_sens > rule_max {
            prop_assert!(!authorized, "sensitivity above max should be denied");
        } else {
            prop_assert!(authorized, "sensitivity at or below max should be allowed");
        }
    }
}

// =============================================================================
// Rotation property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Rotation increments version monotonically.
    #[test]
    fn rotation_increments_version(n in 1u32..=10) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        for i in 0..n {
            let new_ver = broker.rotate_credential("cred-1", 5000 + i as u64 * 100).unwrap();
            prop_assert_eq!(new_ver, 1 + i + 1); // starts at version 1, first rotation -> 2
            broker.complete_rotation("cred-1").unwrap();
        }
        let cred = broker.get_credential("cred-1").unwrap();
        prop_assert_eq!(cred.version, 1 + n);
        prop_assert_eq!(cred.state, CredentialState::Active);
    }

    /// Rotating a revoked credential fails.
    #[test]
    fn rotate_revoked_fails(_dummy in 0u8..1) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        broker.revoke_credential("cred-1", 3000).unwrap();
        let err = broker.rotate_credential("cred-1", 4000).unwrap_err();
        let is_revoked = matches!(err, CredentialBrokerError::CredentialRevoked { .. });
        prop_assert!(is_revoked);
    }

    /// Leases issued during rotation carry the new version.
    #[test]
    fn lease_during_rotation_has_new_version(_dummy in 0u8..1) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        let new_ver = broker.rotate_credential("cred-1", 3000).unwrap();
        // Rotating credential still allows leases
        let scope = CredentialScope::new("github", "repos/x", vec!["read".to_string()]);
        let lease = broker.request_lease("conn-1", "cred-1", scope, 4000).unwrap();
        prop_assert_eq!(lease.credential_version, new_ver);
    }
}

// =============================================================================
// Credential revocation property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Revoking a credential terminates all its active leases.
    #[test]
    fn revoke_credential_terminates_all_leases(n in 1usize..=8) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        for i in 0..n {
            let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
            broker.request_lease(&format!("conn-{i}"), "cred-1", scope, 2000 + i as u64).unwrap();
        }
        let revoked = broker.revoke_credential("cred-1", 5000).unwrap();
        prop_assert_eq!(revoked.len(), n);
        // No active leases remain
        prop_assert_eq!(broker.active_leases_for_credential("cred-1").len(), 0);
        // Credential state is Revoked
        let cred = broker.get_credential("cred-1").unwrap();
        let is_revoked = cred.state == CredentialState::Revoked;
        prop_assert!(is_revoked);
        // active_lease_count reset to 0
        prop_assert_eq!(cred.active_lease_count, 0);
    }

    /// After revocation, new lease requests fail.
    #[test]
    fn no_leases_after_revocation(_dummy in 0u8..1) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        broker.revoke_credential("cred-1", 3000).unwrap();
        let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
        let err = broker.request_lease("conn-1", "cred-1", scope, 4000).unwrap_err();
        let is_revoked_err = matches!(err, CredentialBrokerError::CredentialRevoked { .. });
        prop_assert!(is_revoked_err);
    }
}

// =============================================================================
// Lease limit enforcement property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Exceeding max concurrent leases per connector is rejected.
    #[test]
    fn max_leases_per_connector_enforced(max_leases in 1usize..=5) {
        let mut broker = ConnectorCredentialBroker::new();
        broker.register_provider(SecretProviderConfig {
            provider_id: "p1".to_string(),
            display_name: "P1".to_string(),
            provider_type: "vault".to_string(),
            max_concurrent_leases: 100,
            default_lease_ttl_ms: 60_000,
            supports_rotation: true,
            max_sensitivity: CredentialSensitivity::Critical,
        }, 100).unwrap();
        broker.register_credential(ManagedCredential {
            credential_id: "c1".to_string(),
            provider_id: "p1".to_string(),
            kind: CredentialKind::ApiKey,
            sensitivity: CredentialSensitivity::Low,
            state: CredentialState::Active,
            permitted_scopes: vec![CredentialScope::new("github", "*", vec!["*".to_string()])],
            version: 1,
            created_at_ms: 100,
            expires_at_ms: 0,
            last_rotated_at_ms: 0,
            active_lease_count: 0,
        }, 100).unwrap();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "limited".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
            max_sensitivity: CredentialSensitivity::Critical,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: max_leases,
        });

        // Issue max_leases leases — should all succeed
        for i in 0..max_leases {
            let scope = CredentialScope::new("github", "repos/x", vec!["read".to_string()]);
            broker.request_lease("conn-1", "c1", scope, 2000 + i as u64).unwrap();
        }
        // One more should fail
        let scope = CredentialScope::new("github", "repos/x", vec!["read".to_string()]);
        let err = broker.request_lease("conn-1", "c1", scope, 3000).unwrap_err();
        let is_max_exceeded = matches!(err, CredentialBrokerError::MaxLeasesExceeded { limit, .. } if limit == max_leases);
        prop_assert!(is_max_exceeded);
    }
}

// =============================================================================
// Unavailable provider blocks leases
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Unavailable provider blocks lease issuance; degraded does not.
    #[test]
    fn unavailable_provider_blocks_leases(status in arb_provider_status()) {
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        broker.update_provider_status("prov-1", status, 1500).unwrap();
        let scope = CredentialScope::new("github", "repos/x", vec!["read".to_string()]);
        let result = broker.request_lease("conn-1", "cred-1", scope, 2000);
        if status == ProviderStatus::Unavailable {
            let is_err = result.is_err();
            prop_assert!(is_err, "unavailable provider should block leases");
            let is_unavail = matches!(result.unwrap_err(), CredentialBrokerError::ProviderUnavailable { .. });
            prop_assert!(is_unavail);
        } else {
            let is_ok = result.is_ok();
            prop_assert!(is_ok, "available/degraded provider should allow leases");
        }
    }
}

// =============================================================================
// Audit log bounds property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Audit log never exceeds MAX_AUDIT_EVENTS (1024).
    #[test]
    fn audit_log_bounded(n in 100usize..=1200) {
        let mut broker = ConnectorCredentialBroker::new();
        for i in 0..n {
            broker.register_provider(SecretProviderConfig {
                provider_id: format!("p{i}"),
                display_name: format!("P{i}"),
                provider_type: "env".to_string(),
                max_concurrent_leases: 10,
                default_lease_ttl_ms: 60_000,
                supports_rotation: false,
                max_sensitivity: CredentialSensitivity::Low,
            }, i as u64).unwrap();
        }
        // Each register_provider emits 1 audit event
        prop_assert!(broker.audit_log().len() <= 1024);
    }
}

// =============================================================================
// Telemetry serde roundtrip property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// CredentialBrokerTelemetry survives serde roundtrip.
    #[test]
    fn telemetry_serde_roundtrip(t in arb_telemetry()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: CredentialBrokerTelemetry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    /// CredentialBrokerTelemetrySnapshot survives serde roundtrip.
    #[test]
    fn telemetry_snapshot_serde_roundtrip(snap in arb_telemetry_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let back: CredentialBrokerTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }
}

// =============================================================================
// CredentialScope serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// CredentialScope survives serde roundtrip.
    #[test]
    fn scope_serde_roundtrip(scope in arb_scope()) {
        let json = serde_json::to_string(&scope).unwrap();
        let back: CredentialScope = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(scope, back);
    }
}

// =============================================================================
// CredentialAccessRule matching property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Wildcard connector + wildcard scope rule matches any request.
    #[test]
    fn wildcard_rule_matches_all(
        connector in arb_connector_id(),
        scope in arb_scope(),
    ) {
        let rule = CredentialAccessRule {
            rule_id: "wild".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new(
                scope.provider.clone(),
                "*",
                vec!["*".to_string()],
            ),
            max_sensitivity: CredentialSensitivity::Critical,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 10,
        };
        prop_assert!(rule.matches(&connector, &scope));
    }

    /// Rule with specific connector_pattern rejects mismatches.
    #[test]
    fn specific_rule_rejects_mismatch(
        target in arb_connector_id(),
    ) {
        let rule = CredentialAccessRule {
            rule_id: "specific".to_string(),
            connector_pattern: "conn-only-this-one".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
            max_sensitivity: CredentialSensitivity::Critical,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 10,
        };
        let scope = CredentialScope::new("github", "repos/x", vec!["read".to_string()]);
        let expected = target == "conn-only-this-one";
        prop_assert_eq!(rule.matches(&target, &scope), expected);
    }
}

// =============================================================================
// Telemetry snapshot accuracy property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Telemetry snapshot active counts are accurate after mixed operations.
    #[test]
    fn telemetry_snapshot_accuracy(
        n_lease in 1usize..=6,
        n_revoke in 0usize..=3,
    ) {
        let n_revoke = n_revoke.min(n_lease);
        let mut broker = setup_broker_for_leasing(
            CredentialSensitivity::Critical,
            CredentialSensitivity::Medium,
            60_000,
        );
        let mut lease_ids = Vec::new();
        for i in 0..n_lease {
            let scope = CredentialScope::new("github", "repos/foo", vec!["read".to_string()]);
            let lease = broker.request_lease(&format!("conn-{i}"), "cred-1", scope, 2000 + i as u64).unwrap();
            lease_ids.push(lease.lease_id);
        }
        for i in 0..n_revoke {
            broker.revoke_lease(&lease_ids[i], 5000 + i as u64).unwrap();
        }
        let snap = broker.telemetry_snapshot(9000);
        prop_assert_eq!(snap.active_leases, (n_lease - n_revoke) as u32);
        prop_assert_eq!(snap.counters.leases_issued, n_lease as u64);
        prop_assert_eq!(snap.counters.leases_revoked, n_revoke as u64);
        prop_assert_eq!(snap.active_credentials, 1);
        prop_assert_eq!(snap.active_providers, 1);
    }
}

// =============================================================================
// Error variant property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// All error variants produce non-empty Display messages.
    #[test]
    fn error_display_nonempty(
        id in "[a-z]{3,10}",
        limit in 1usize..=100,
    ) {
        let errors: Vec<CredentialBrokerError> = vec![
            CredentialBrokerError::ProviderNotFound { provider_id: id.clone() },
            CredentialBrokerError::CredentialNotFound { credential_id: id.clone() },
            CredentialBrokerError::CredentialRevoked { credential_id: id.clone() },
            CredentialBrokerError::CredentialExpired { credential_id: id.clone() },
            CredentialBrokerError::NotAuthorized { connector_id: id.clone(), scope: "test".to_string() },
            CredentialBrokerError::MaxLeasesExceeded { connector_id: id.clone(), limit },
            CredentialBrokerError::LeaseExpired { lease_id: id.clone() },
            CredentialBrokerError::ProviderUnavailable { provider_id: id, reason: "down".to_string() },
        ];
        for e in &errors {
            let msg = e.to_string();
            prop_assert!(!msg.is_empty());
        }
    }
}

// =============================================================================
// CredentialAuditType display property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// All audit type variants produce snake_case display strings.
    #[test]
    fn audit_type_display_snake_case(
        variant in prop_oneof![
            Just(CredentialAuditType::CredentialRegistered),
            Just(CredentialAuditType::LeaseIssued),
            Just(CredentialAuditType::LeaseExpired),
            Just(CredentialAuditType::LeaseRevoked),
            Just(CredentialAuditType::CredentialRotated),
            Just(CredentialAuditType::CredentialRevoked),
            Just(CredentialAuditType::CredentialExpired),
            Just(CredentialAuditType::AccessDenied),
            Just(CredentialAuditType::ProviderRegistered),
            Just(CredentialAuditType::ProviderStatusChanged),
        ]
    ) {
        let display = variant.to_string();
        prop_assert!(!display.is_empty());
        prop_assert!(display.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    }
}

// =============================================================================
// Credential expiration at lease time
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A credential with expires_at_ms in the past blocks lease issuance.
    #[test]
    fn expired_credential_blocks_lease(
        expires_at in 1000u64..=5000,
        request_at in 5001u64..=10000,
    ) {
        let mut broker = ConnectorCredentialBroker::new();
        broker.register_provider(SecretProviderConfig {
            provider_id: "p1".to_string(),
            display_name: "P1".to_string(),
            provider_type: "vault".to_string(),
            max_concurrent_leases: 100,
            default_lease_ttl_ms: 60_000,
            supports_rotation: true,
            max_sensitivity: CredentialSensitivity::Critical,
        }, 100).unwrap();
        broker.register_credential(ManagedCredential {
            credential_id: "c1".to_string(),
            provider_id: "p1".to_string(),
            kind: CredentialKind::ApiKey,
            sensitivity: CredentialSensitivity::Low,
            state: CredentialState::Active,
            permitted_scopes: vec![CredentialScope::new("github", "*", vec!["*".to_string()])],
            version: 1,
            created_at_ms: 100,
            expires_at_ms: expires_at,
            last_rotated_at_ms: 0,
            active_lease_count: 0,
        }, 100).unwrap();
        broker.add_access_rule(CredentialAccessRule {
            rule_id: "r1".to_string(),
            connector_pattern: "*".to_string(),
            permitted_scope: CredentialScope::new("github", "*", vec!["*".to_string()]),
            max_sensitivity: CredentialSensitivity::Critical,
            max_lease_ttl_ms: 0,
            max_concurrent_leases: 10,
        });
        let scope = CredentialScope::new("github", "repos/x", vec!["read".to_string()]);
        let err = broker.request_lease("conn-1", "c1", scope, request_at).unwrap_err();
        let is_expired = matches!(err, CredentialBrokerError::CredentialExpired { .. });
        prop_assert!(is_expired);
    }
}
