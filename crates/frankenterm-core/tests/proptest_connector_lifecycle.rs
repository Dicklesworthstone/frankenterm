//! Property-based tests for connector_lifecycle module (ft-3681t.5.4).

use std::collections::BTreeMap;

use frankenterm_core::connector_host_runtime::{ConnectorCapability, ConnectorLifecyclePhase};
use frankenterm_core::connector_lifecycle::*;
use frankenterm_core::connector_registry::{ConnectorManifest, TrustPolicy};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_admin_state() -> impl Strategy<Value = AdminState> {
    prop_oneof![
        Just(AdminState::Enabled),
        Just(AdminState::Disabled),
        Just(AdminState::Upgrading),
        Just(AdminState::Uninstalling),
    ]
}

fn arb_lifecycle_phase() -> impl Strategy<Value = ConnectorLifecyclePhase> {
    prop_oneof![
        Just(ConnectorLifecyclePhase::Stopped),
        Just(ConnectorLifecyclePhase::Starting),
        Just(ConnectorLifecyclePhase::Running),
        Just(ConnectorLifecyclePhase::Degraded),
        Just(ConnectorLifecyclePhase::Failed),
    ]
}

fn arb_upgrade_strategy() -> impl Strategy<Value = UpgradeStrategy> {
    prop_oneof![
        Just(UpgradeStrategy::StopAndReplace),
        Just(UpgradeStrategy::BlueGreen),
        Just(UpgradeStrategy::RollingDrain),
    ]
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

fn arb_connector_id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("slack-connector".to_string()),
        Just("github-events".to_string()),
        Just("datadog-metrics".to_string()),
        Just("jira-bridge".to_string()),
        Just("pagerduty-alerts".to_string()),
    ]
}

fn arb_version() -> impl Strategy<Value = String> {
    (1u32..10, 0u32..20, 0u32..50).prop_map(|(major, minor, patch)| {
        format!("{major}.{minor}.{patch}")
    })
}

fn test_manifest(id: &str, version: &str) -> ConnectorManifest {
    ConnectorManifest {
        schema_version: 1,
        package_id: id.to_string(),
        version: version.to_string(),
        display_name: format!("Test {id}"),
        description: "test connector".to_string(),
        author: "test-publisher".to_string(),
        min_ft_version: None,
        sha256_digest: "a".repeat(64),
        required_capabilities: vec![ConnectorCapability::Invoke],
        publisher_signature: Some("sig".to_string()),
        transparency_token: None,
        created_at_ms: 1000,
        metadata: BTreeMap::new(),
    }
}

fn trusted_manager() -> ConnectorLifecycleManager {
    let mut config = LifecycleManagerConfig::default();
    config
        .trust_policy
        .trusted_publishers
        .push("test-publisher".to_string());
    ConnectorLifecycleManager::new(config)
}

fn trusted_manager_with_config(
    max_restarts: u32,
    window_secs: u64,
    cooldown_ms: u64,
    max_connectors: usize,
) -> ConnectorLifecycleManager {
    let mut config = LifecycleManagerConfig::default();
    config
        .trust_policy
        .trusted_publishers
        .push("test-publisher".to_string());
    config.restart_policy.max_restarts = max_restarts;
    config.restart_policy.window_secs = window_secs;
    config.restart_policy.cooldown_ms = cooldown_ms;
    config.max_managed_connectors = max_connectors;
    ConnectorLifecycleManager::new(config)
}

// =============================================================================
// AdminState Properties
// =============================================================================

proptest! {
    #[test]
    fn admin_state_serde_roundtrip(state in arb_admin_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: AdminState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, back);
    }

    #[test]
    fn admin_state_can_start_only_for_enabled(state in arb_admin_state()) {
        let can_start = state.can_start();
        let is_enabled = state == AdminState::Enabled;
        prop_assert_eq!(can_start, is_enabled,
            "can_start should be true only for Enabled, got {} for {:?}", can_start, state);
    }

    #[test]
    fn admin_state_display_matches_as_str(state in arb_admin_state()) {
        let display = format!("{state}");
        prop_assert_eq!(display.as_str(), state.as_str());
    }
}

// =============================================================================
// UpgradeStrategy Properties
// =============================================================================

proptest! {
    #[test]
    fn upgrade_strategy_serde_roundtrip(strat in arb_upgrade_strategy()) {
        let json = serde_json::to_string(&strat).unwrap();
        let back: UpgradeStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(strat, back);
    }
}

// =============================================================================
// LifecycleIntent Properties
// =============================================================================

proptest! {
    #[test]
    fn intent_connector_id_consistent(
        id in arb_connector_id(),
        version in arb_version(),
    ) {
        let manifest = test_manifest(&id, &version);

        let intents = vec![
            LifecycleIntent::Install { manifest: manifest.clone() },
            LifecycleIntent::Update { connector_id: id.clone(), manifest: manifest.clone() },
            LifecycleIntent::Enable { connector_id: id.clone() },
            LifecycleIntent::Disable { connector_id: id.clone(), reason: "test".to_string() },
            LifecycleIntent::Restart { connector_id: id.clone() },
            LifecycleIntent::Uninstall { connector_id: id.clone() },
            LifecycleIntent::Rollback { connector_id: id.clone() },
        ];

        for intent in &intents {
            prop_assert_eq!(intent.connector_id(), id.as_str(),
                "connector_id() mismatch for {:?}", intent.op_name());
        }
    }

    #[test]
    fn intent_op_name_unique_per_variant(id in arb_connector_id()) {
        let manifest = test_manifest(&id, "1.0.0");
        let intents: Vec<(&str, LifecycleIntent)> = vec![
            ("install", LifecycleIntent::Install { manifest }),
            ("update", LifecycleIntent::Update { connector_id: id.clone(), manifest: test_manifest(&id, "2.0.0") }),
            ("enable", LifecycleIntent::Enable { connector_id: id.clone() }),
            ("disable", LifecycleIntent::Disable { connector_id: id.clone(), reason: "r".into() }),
            ("restart", LifecycleIntent::Restart { connector_id: id.clone() }),
            ("uninstall", LifecycleIntent::Uninstall { connector_id: id.clone() }),
            ("rollback", LifecycleIntent::Rollback { connector_id: id }),
        ];

        for (expected, intent) in &intents {
            prop_assert_eq!(intent.op_name(), *expected);
        }
    }

    #[test]
    fn intent_serde_roundtrip(id in arb_connector_id()) {
        let manifest = test_manifest(&id, "1.0.0");
        let intents = vec![
            LifecycleIntent::Install { manifest },
            LifecycleIntent::Enable { connector_id: id.clone() },
            LifecycleIntent::Disable { connector_id: id.clone(), reason: "test".to_string() },
            LifecycleIntent::Restart { connector_id: id.clone() },
            LifecycleIntent::Uninstall { connector_id: id.clone() },
            LifecycleIntent::Rollback { connector_id: id },
        ];

        for intent in intents {
            let json = serde_json::to_string(&intent).unwrap();
            let back: LifecycleIntent = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(intent, back);
        }
    }
}

// =============================================================================
// RestartPolicy Properties
// =============================================================================

proptest! {
    #[test]
    fn restart_policy_strict_tighter_than_lenient(_dummy in 0u8..1) {
        let strict = RestartPolicy::strict();
        let lenient = RestartPolicy::lenient();
        prop_assert!(strict.max_restarts <= lenient.max_restarts,
            "strict max_restarts {} should be <= lenient {}", strict.max_restarts, lenient.max_restarts);
        prop_assert!(strict.cooldown_ms >= lenient.cooldown_ms,
            "strict cooldown {} should be >= lenient {}", strict.cooldown_ms, lenient.cooldown_ms);
    }

    #[test]
    fn restart_policy_serde_roundtrip(
        max in 1u32..100,
        window in 10u64..3600,
        cooldown in 100u64..30_000,
    ) {
        let policy = RestartPolicy { max_restarts: max, window_secs: window, cooldown_ms: cooldown };
        let json = serde_json::to_string(&policy).unwrap();
        let back: RestartPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(policy, back);
    }
}

// =============================================================================
// Install Properties
// =============================================================================

proptest! {
    #[test]
    fn install_always_creates_enabled_stopped(
        id in arb_connector_id(),
        version in arb_version(),
        now_ms in 1000u64..1_000_000,
    ) {
        let mut mgr = trusted_manager();
        let manifest = test_manifest(&id, &version);
        let result = mgr.execute(LifecycleIntent::Install { manifest }, now_ms).unwrap();
        prop_assert!(result.success);
        prop_assert_eq!(result.admin_state, AdminState::Enabled);
        prop_assert_eq!(result.runtime_phase, ConnectorLifecyclePhase::Stopped);
        prop_assert_eq!(mgr.count(), 1);

        let mc = mgr.get(&id).unwrap();
        prop_assert_eq!(mc.admin_state, AdminState::Enabled);
        prop_assert_eq!(mc.runtime_phase, ConnectorLifecyclePhase::Stopped);
        prop_assert_eq!(mc.installed_at_ms, now_ms);
        prop_assert!(mc.previous_version.is_none());
        prop_assert!(mc.rollback_manifest.is_none());
    }

    #[test]
    fn install_duplicate_always_fails(
        id in arb_connector_id(),
        now_ms in 1000u64..500_000,
    ) {
        let mut mgr = trusted_manager();
        let manifest = test_manifest(&id, "1.0.0");
        mgr.execute(LifecycleIntent::Install { manifest: manifest.clone() }, now_ms).unwrap();
        let err = mgr.execute(LifecycleIntent::Install { manifest }, now_ms + 1000).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::AlreadyInstalled { .. });
        prop_assert!(check, "expected AlreadyInstalled, got {:?}", err);
    }

    #[test]
    fn install_capacity_enforced(
        cap in 1usize..5,
        now_ms in 1000u64..100_000,
    ) {
        let mut mgr = trusted_manager_with_config(5, 300, 0, cap);
        for i in 0..cap {
            let manifest = test_manifest(&format!("conn-{i}"), "1.0.0");
            mgr.execute(LifecycleIntent::Install { manifest }, now_ms + i as u64 * 100).unwrap();
        }
        prop_assert_eq!(mgr.count(), cap);

        let manifest = test_manifest("one-too-many", "1.0.0");
        let err = mgr.execute(LifecycleIntent::Install { manifest }, now_ms + 99999).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::UpgradePreconditionFailed { .. });
        prop_assert!(check, "expected capacity error, got {:?}", err);
    }
}

// =============================================================================
// Update Properties
// =============================================================================

proptest! {
    #[test]
    fn update_changes_version_and_preserves_rollback(
        id in arb_connector_id(),
        v1_minor in 0u32..10,
        v2_minor in 11u32..20,
    ) {
        let v1 = format!("1.{v1_minor}.0");
        let v2 = format!("1.{v2_minor}.0");
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, &v1) }, 1000).unwrap();

        let result = mgr.execute(
            LifecycleIntent::Update { connector_id: id.clone(), manifest: test_manifest(&id, &v2) },
            2000,
        ).unwrap();

        prop_assert!(result.success);
        let mc = mgr.get(&id).unwrap();
        prop_assert_eq!(&mc.version, &v2);
        prop_assert_eq!(mc.previous_version.as_deref(), Some(v1.as_str()));
        prop_assert!(mc.rollback_manifest.is_some());
        prop_assert_eq!(mc.admin_state, AdminState::Enabled);
    }

    #[test]
    fn update_same_version_rejected(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        let err = mgr.execute(
            LifecycleIntent::Update { connector_id: id, manifest: test_manifest("ignored", "1.0.0") },
            2000,
        );
        // Either UpgradePreconditionFailed (same version) or mismatch error
        prop_assert!(err.is_err());
    }

    #[test]
    fn update_not_installed_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        let err = mgr.execute(
            LifecycleIntent::Update { connector_id: id, manifest: test_manifest("x", "2.0.0") },
            1000,
        ).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::NotInstalled { .. });
        prop_assert!(check);
    }
}

// =============================================================================
// Enable / Disable Properties
// =============================================================================

proptest! {
    #[test]
    fn disable_then_enable_roundtrips(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();

        mgr.execute(
            LifecycleIntent::Disable { connector_id: id.clone(), reason: "test".to_string() },
            2000,
        ).unwrap();
        let mc = mgr.get(&id).unwrap();
        prop_assert_eq!(mc.admin_state, AdminState::Disabled);
        prop_assert_eq!(mc.runtime_phase, ConnectorLifecyclePhase::Stopped);

        mgr.execute(LifecycleIntent::Enable { connector_id: id.clone() }, 3000).unwrap();
        let mc = mgr.get(&id).unwrap();
        prop_assert_eq!(mc.admin_state, AdminState::Enabled);
    }

    #[test]
    fn enable_uninstalling_always_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();

        // Force state to Uninstalling (not reachable via normal disable->enable)
        mgr.execute(LifecycleIntent::Uninstall { connector_id: id.clone() }, 2000).unwrap();
        let err = mgr.execute(LifecycleIntent::Enable { connector_id: id }, 3000);
        prop_assert!(err.is_err());
    }
}

// =============================================================================
// Restart Properties
// =============================================================================

proptest! {
    #[test]
    fn restart_transitions_to_starting(id in arb_connector_id()) {
        let mut mgr = trusted_manager_with_config(10, 300, 0, 64);
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        let result = mgr.execute(LifecycleIntent::Restart { connector_id: id.clone() }, 2000).unwrap();
        prop_assert_eq!(result.runtime_phase, ConnectorLifecyclePhase::Starting);
    }

    #[test]
    fn restart_rate_limit_enforced(
        max_restarts in 1u32..5,
        id in arb_connector_id(),
    ) {
        let mut mgr = trusted_manager_with_config(max_restarts, 600, 0, 64);
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();

        // Use up all restarts
        for i in 0..max_restarts {
            mgr.execute(
                LifecycleIntent::Restart { connector_id: id.clone() },
                2000 + i as u64 * 100,
            ).unwrap();
        }

        // Next restart should fail
        let err = mgr.execute(
            LifecycleIntent::Restart { connector_id: id },
            2000 + max_restarts as u64 * 100,
        ).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::RestartLimitExceeded { .. });
        prop_assert!(check, "expected RestartLimitExceeded, got {:?}", err);
    }

    #[test]
    fn restart_window_reset_allows_new_restarts(
        max_restarts in 1u32..5,
        window_secs in 5u64..30,
    ) {
        let id = "test-conn";
        let mut mgr = trusted_manager_with_config(max_restarts, window_secs, 0, 64);
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(id, "1.0.0") }, 1000).unwrap();

        // Exhaust restarts
        for i in 0..max_restarts {
            mgr.execute(
                LifecycleIntent::Restart { connector_id: id.to_string() },
                2000 + i as u64 * 100,
            ).unwrap();
        }

        // After window expires, restarts allowed again
        let after_window = 2000 + window_secs * 1000 + 1;
        let result = mgr.execute(
            LifecycleIntent::Restart { connector_id: id.to_string() },
            after_window,
        );
        prop_assert!(result.is_ok(),
            "restart should succeed after window reset, got {:?}", result.err());
    }

    #[test]
    fn restart_disabled_connector_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager_with_config(10, 300, 0, 64);
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        mgr.execute(
            LifecycleIntent::Disable { connector_id: id.clone(), reason: "test".into() },
            2000,
        ).unwrap();
        let err = mgr.execute(LifecycleIntent::Restart { connector_id: id }, 3000).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::InvalidTransition { .. });
        prop_assert!(check);
    }

    #[test]
    fn can_restart_agrees_with_execute(id in arb_connector_id()) {
        let mut mgr = trusted_manager_with_config(10, 300, 0, 64);
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();

        let can = mgr.can_restart(&id, 2000);
        let result = mgr.execute(LifecycleIntent::Restart { connector_id: id }, 2000);

        // If can_restart says yes, execute should succeed (in fresh state)
        if can {
            prop_assert!(result.is_ok(), "can_restart=true but execute failed: {:?}", result.err());
        }
    }
}

// =============================================================================
// Rollback Properties
// =============================================================================

proptest! {
    #[test]
    fn rollback_restores_previous_version(
        id in arb_connector_id(),
        v1_minor in 0u32..10,
        v2_minor in 11u32..20,
    ) {
        let v1 = format!("1.{v1_minor}.0");
        let v2 = format!("1.{v2_minor}.0");
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, &v1) }, 1000).unwrap();
        mgr.execute(
            LifecycleIntent::Update { connector_id: id.clone(), manifest: test_manifest(&id, &v2) },
            2000,
        ).unwrap();
        mgr.execute(LifecycleIntent::Rollback { connector_id: id.clone() }, 3000).unwrap();

        let mc = mgr.get(&id).unwrap();
        prop_assert_eq!(&mc.version, &v1);
        prop_assert!(mc.previous_version.is_none());
        prop_assert!(mc.rollback_manifest.is_none());
        prop_assert_eq!(mc.admin_state, AdminState::Enabled);
    }

    #[test]
    fn rollback_without_update_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        let err = mgr.execute(LifecycleIntent::Rollback { connector_id: id }, 2000).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::RollbackFailed { .. });
        prop_assert!(check);
    }

    #[test]
    fn double_rollback_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        mgr.execute(
            LifecycleIntent::Update { connector_id: id.clone(), manifest: test_manifest(&id, "2.0.0") },
            2000,
        ).unwrap();
        mgr.execute(LifecycleIntent::Rollback { connector_id: id.clone() }, 3000).unwrap();
        let err = mgr.execute(LifecycleIntent::Rollback { connector_id: id }, 4000).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::RollbackFailed { .. });
        prop_assert!(check);
    }
}

// =============================================================================
// Uninstall Properties
// =============================================================================

proptest! {
    #[test]
    fn uninstall_removes_connector(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        prop_assert_eq!(mgr.count(), 1);

        mgr.execute(LifecycleIntent::Uninstall { connector_id: id.clone() }, 2000).unwrap();
        prop_assert_eq!(mgr.count(), 0);
        prop_assert!(mgr.get(&id).is_none());
    }

    #[test]
    fn uninstall_not_installed_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        let err = mgr.execute(LifecycleIntent::Uninstall { connector_id: id }, 1000).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::NotInstalled { .. });
        prop_assert!(check);
    }
}

// =============================================================================
// Phase Change Properties
// =============================================================================

proptest! {
    #[test]
    fn phase_change_updates_runtime(
        phase in arb_lifecycle_phase(),
        id in arb_connector_id(),
    ) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        mgr.notify_phase_change(&id, phase, 2000).unwrap();
        prop_assert_eq!(mgr.get(&id).unwrap().runtime_phase, phase);
    }

    #[test]
    fn phase_change_unknown_fails(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        let err = mgr.notify_phase_change(&id, ConnectorLifecyclePhase::Running, 1000).unwrap_err();
        let check = matches!(err, ConnectorLifecycleError::NotInstalled { .. });
        prop_assert!(check);
    }
}

// =============================================================================
// Op Counter Properties
// =============================================================================

proptest! {
    #[test]
    fn op_counter_monotonically_increases(
        n_ops in 1usize..10,
    ) {
        let mut mgr = trusted_manager_with_config(100, 300, 0, 64);
        let ids: Vec<String> = (0..n_ops).map(|i| format!("conn-{i}")).collect();

        for (i, id) in ids.iter().enumerate() {
            mgr.execute(
                LifecycleIntent::Install { manifest: test_manifest(id, "1.0.0") },
                1000 + i as u64 * 100,
            ).unwrap();
            prop_assert_eq!(mgr.op_counter(), (i + 1) as u64,
                "op_counter should be {} after {} operations", i + 1, i + 1);
        }
    }
}

// =============================================================================
// Audit Log Properties
// =============================================================================

proptest! {
    #[test]
    fn audit_log_bounded_after_many_operations(
        n_phase_changes in 50usize..200,
    ) {
        let mut mgr = trusted_manager();
        let id = "audit-test";
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(id, "1.0.0") }, 1000).unwrap();

        for i in 0..n_phase_changes {
            let phase = if i % 2 == 0 {
                ConnectorLifecyclePhase::Running
            } else {
                ConnectorLifecyclePhase::Degraded
            };
            mgr.notify_phase_change(id, phase, 2000 + i as u64).unwrap();
        }

        let log = mgr.audit_log(id).unwrap();
        prop_assert!(log.len() <= 100,
            "audit log should be bounded to 100, got {}", log.len());
    }

    #[test]
    fn audit_log_records_install(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        let log = mgr.audit_log(&id).unwrap();
        prop_assert!(!log.is_empty());
        prop_assert_eq!(&log[0].operation, "install");
    }
}

// =============================================================================
// Summary Consistency Properties
// =============================================================================

proptest! {
    #[test]
    fn summary_total_equals_count(n in 1usize..8) {
        let mut mgr = trusted_manager_with_config(10, 300, 0, 64);
        for i in 0..n {
            mgr.execute(
                LifecycleIntent::Install { manifest: test_manifest(&format!("c-{i}"), "1.0.0") },
                1000 + i as u64 * 100,
            ).unwrap();
        }
        let summary = mgr.summary();
        prop_assert_eq!(summary.total as usize, mgr.count());
        prop_assert_eq!(summary.total as usize, n);
    }

    #[test]
    fn summary_admin_state_counts_consistent(n in 2usize..6) {
        let mut mgr = trusted_manager_with_config(10, 300, 0, 64);
        for i in 0..n {
            mgr.execute(
                LifecycleIntent::Install { manifest: test_manifest(&format!("c-{i}"), "1.0.0") },
                1000 + i as u64 * 100,
            ).unwrap();
        }
        // Disable first connector
        mgr.execute(
            LifecycleIntent::Disable { connector_id: "c-0".to_string(), reason: "test".into() },
            5000,
        ).unwrap();

        let summary = mgr.summary();
        prop_assert_eq!(summary.disabled, 1);
        prop_assert_eq!(summary.enabled, (n - 1) as u32);
        // enabled + disabled should equal total (no upgrading/uninstalling)
        prop_assert_eq!(summary.enabled + summary.disabled, summary.total);
    }
}

// =============================================================================
// Serde Roundtrip Properties
// =============================================================================

proptest! {
    #[test]
    fn lifecycle_result_serde_roundtrip(
        id in arb_connector_id(),
        state in arb_admin_state(),
        phase in arb_lifecycle_phase(),
        at_ms in 0u64..1_000_000,
    ) {
        let result = LifecycleResult {
            connector_id: id,
            operation: "test".to_string(),
            success: true,
            admin_state: state,
            runtime_phase: phase,
            detail: "detail".to_string(),
            at_ms,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: LifecycleResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result, back);
    }

    #[test]
    fn lifecycle_manager_summary_serde_roundtrip(
        total in 0u32..100,
        enabled in 0u32..50,
        disabled in 0u32..50,
        running in 0u32..50,
        stopped in 0u32..50,
        degraded in 0u32..50,
        failed in 0u32..50,
    ) {
        let summary = LifecycleManagerSummary {
            total, enabled, disabled, running, stopped, degraded, failed,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: LifecycleManagerSummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(summary, back);
    }

    #[test]
    fn managed_connector_serde_roundtrip_via_manager(id in arb_connector_id()) {
        let mut mgr = trusted_manager();
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        let mc = mgr.get(&id).unwrap();
        let json = serde_json::to_string(mc).unwrap();
        let back: ManagedConnector = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&mc.connector_id, &back.connector_id);
        prop_assert_eq!(&mc.version, &back.version);
        prop_assert_eq!(mc.admin_state, back.admin_state);
        prop_assert_eq!(mc.runtime_phase, back.runtime_phase);
    }
}

// =============================================================================
// Lifecycle Sequence Invariants
// =============================================================================

proptest! {
    #[test]
    fn install_update_rollback_preserves_original_state(
        id in arb_connector_id(),
        caps in prop::collection::vec(arb_capability(), 1..4),
    ) {
        let mut mgr = trusted_manager();

        let mut m1 = test_manifest(&id, "1.0.0");
        m1.required_capabilities = caps.clone();
        m1.display_name = "Original".to_string();
        mgr.execute(LifecycleIntent::Install { manifest: m1 }, 1000).unwrap();

        let original_version = mgr.get(&id).unwrap().version.clone();
        let original_display = mgr.get(&id).unwrap().display_name.clone();
        let original_caps = mgr.get(&id).unwrap().granted_capabilities.clone();

        let mut m2 = test_manifest(&id, "2.0.0");
        m2.required_capabilities = vec![ConnectorCapability::NetworkEgress];
        m2.display_name = "Updated".to_string();
        mgr.execute(
            LifecycleIntent::Update { connector_id: id.clone(), manifest: m2 },
            2000,
        ).unwrap();

        mgr.execute(LifecycleIntent::Rollback { connector_id: id.clone() }, 3000).unwrap();

        let mc = mgr.get(&id).unwrap();
        prop_assert_eq!(&mc.version, &original_version);
        prop_assert_eq!(&mc.display_name, &original_display);
        prop_assert_eq!(&mc.granted_capabilities, &original_caps);
    }

    #[test]
    fn full_lifecycle_sequence(id in arb_connector_id()) {
        let mut mgr = trusted_manager_with_config(10, 300, 0, 64);

        // Install
        mgr.execute(LifecycleIntent::Install { manifest: test_manifest(&id, "1.0.0") }, 1000).unwrap();
        prop_assert_eq!(mgr.count(), 1);

        // Phase change to running
        mgr.notify_phase_change(&id, ConnectorLifecyclePhase::Running, 1500).unwrap();
        prop_assert_eq!(mgr.get(&id).unwrap().runtime_phase, ConnectorLifecyclePhase::Running);

        // Disable
        mgr.execute(
            LifecycleIntent::Disable { connector_id: id.clone(), reason: "maint".into() },
            2000,
        ).unwrap();
        prop_assert_eq!(mgr.get(&id).unwrap().admin_state, AdminState::Disabled);

        // Re-enable
        mgr.execute(LifecycleIntent::Enable { connector_id: id.clone() }, 3000).unwrap();
        prop_assert_eq!(mgr.get(&id).unwrap().admin_state, AdminState::Enabled);

        // Update
        mgr.execute(
            LifecycleIntent::Update { connector_id: id.clone(), manifest: test_manifest(&id, "2.0.0") },
            4000,
        ).unwrap();
        prop_assert_eq!(&mgr.get(&id).unwrap().version, "2.0.0");

        // Rollback
        mgr.execute(LifecycleIntent::Rollback { connector_id: id.clone() }, 5000).unwrap();
        prop_assert_eq!(&mgr.get(&id).unwrap().version, "1.0.0");

        // Restart
        mgr.execute(LifecycleIntent::Restart { connector_id: id.clone() }, 6000).unwrap();
        prop_assert_eq!(mgr.get(&id).unwrap().runtime_phase, ConnectorLifecyclePhase::Starting);

        // Uninstall
        mgr.execute(LifecycleIntent::Uninstall { connector_id: id.clone() }, 7000).unwrap();
        prop_assert_eq!(mgr.count(), 0);

        // Verify ops
        prop_assert!(mgr.op_counter() >= 7);
    }
}
