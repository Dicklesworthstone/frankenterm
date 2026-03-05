use frankenterm_core::connector_host_runtime::{
    ConnectorCapability, ConnectorFailureClass, ConnectorHostConfig, ConnectorHostRuntime,
    ConnectorHostRuntimeError, ConnectorLifecyclePhase, ConnectorOperationRequest,
    ConnectorProtocolVersion, ConnectorRuntimeUsage, StartupProbeResult,
};

fn usage_within_budget() -> ConnectorRuntimeUsage {
    ConnectorRuntimeUsage {
        cpu_millis_in_window: 120,
        memory_bytes: 128 * 1024 * 1024,
        io_bytes_in_window: 512 * 1024,
        inflight_ops: 8,
    }
}

#[test]
fn connector_host_runtime_integration_degraded_path_and_recovery_contract() {
    let mut runtime = ConnectorHostRuntime::new(ConnectorHostConfig::default()).unwrap();
    runtime.start(1_000).unwrap();
    runtime.record_heartbeat(1_010).unwrap();

    let degrade_err = runtime
        .observe_usage(
            1_020,
            ConnectorRuntimeUsage {
                cpu_millis_in_window: 900,
                ..usage_within_budget()
            },
        )
        .unwrap_err();
    assert_eq!(
        degrade_err,
        ConnectorHostRuntimeError::BudgetExceeded {
            dimension: "cpu_millis_per_second".to_string(),
        }
    );

    let degraded = runtime.health_snapshot(1_040);
    assert_eq!(degraded.phase, ConnectorLifecyclePhase::Degraded);
    assert!(degraded.is_live);
    assert!(!degraded.is_ready);
    assert_eq!(
        degraded
            .last_failure
            .as_ref()
            .expect("failure should be present in degraded phase")
            .reason_code,
        "budget_exceeded.cpu_millis_per_second"
    );

    runtime.record_heartbeat(1_050).unwrap();
    runtime.observe_usage(1_060, usage_within_budget()).unwrap();

    let recovered = runtime.health_snapshot(1_080);
    assert_eq!(recovered.phase, ConnectorLifecyclePhase::Running);
    assert!(recovered.is_live);
    assert!(recovered.is_ready);

    let envelope = runtime
        .build_operation_envelope(1_090, "connector.invoke", "corr-int-1")
        .unwrap();
    assert_eq!(
        envelope.protocol_version,
        ConnectorProtocolVersion::new(1, 0, 0)
    );
    assert_eq!(envelope.correlation_id, "corr-int-1");

    let transition_json = serde_json::to_string(&runtime.transition_history()).unwrap();
    assert!(transition_json.contains("lifecycle.degraded.budget_exceeded"));
    assert!(transition_json.contains("lifecycle.degraded.recovered"));

    let health_json = serde_json::to_string(&recovered).unwrap();
    assert!(health_json.contains("\"is_ready\":true"));
}

#[test]
fn connector_host_runtime_integration_upgrade_and_failed_start_recovery() {
    let mut config = ConnectorHostConfig::default();
    config.host_id = "connector-host-int".to_string();
    let mut runtime = ConnectorHostRuntime::new(config).unwrap();

    let start_err = runtime
        .start_with_probe(
            10,
            StartupProbeResult::failed(ConnectorFailureClass::Auth, "token_missing"),
        )
        .unwrap_err();
    assert_eq!(
        start_err,
        ConnectorHostRuntimeError::StartupProbeFailed {
            class: ConnectorFailureClass::Auth,
            reason_code: "token_missing".to_string(),
        }
    );
    assert_eq!(
        runtime.health_snapshot(20).phase,
        ConnectorLifecyclePhase::Failed
    );

    runtime
        .restart_with_probe(30, StartupProbeResult::healthy())
        .unwrap();
    runtime.observe_usage(40, usage_within_budget()).unwrap();
    let pre_upgrade = runtime
        .build_operation_envelope(50, "connector.ping", "corr-pre-upgrade")
        .unwrap();
    assert_eq!(
        pre_upgrade.protocol_version,
        ConnectorProtocolVersion::new(1, 0, 0)
    );

    runtime
        .upgrade_and_restart(
            60,
            ConnectorProtocolVersion::new(1, 2, 0),
            StartupProbeResult::healthy(),
        )
        .unwrap();
    runtime.observe_usage(70, usage_within_budget()).unwrap();

    let post_upgrade = runtime
        .build_operation_envelope(80, "connector.ping", "corr-post-upgrade")
        .unwrap();
    assert_eq!(
        post_upgrade.protocol_version,
        ConnectorProtocolVersion::new(1, 2, 0)
    );
    assert!(post_upgrade.operation_id > pre_upgrade.operation_id);

    let snapshot = runtime.health_snapshot(85);
    assert_eq!(snapshot.phase, ConnectorLifecyclePhase::Running);
    assert!(snapshot.is_live);
    assert!(snapshot.is_ready);

    let history = runtime.transition_history();
    assert!(
        history
            .iter()
            .any(|record| record.reason_code == "lifecycle.upgrade.applied")
    );
}

#[test]
fn connector_host_runtime_integration_sandbox_fail_closed_contract() {
    let mut config = ConnectorHostConfig::default();
    config.sandbox.capability_envelope.allowed_capabilities = vec![ConnectorCapability::ReadState];
    let mut runtime = ConnectorHostRuntime::new(config).unwrap();
    runtime.start(100).unwrap();
    runtime.observe_usage(120, usage_within_budget()).unwrap();

    let err = runtime
        .authorize_operation(
            130,
            ConnectorOperationRequest::new(
                "connector.invoke",
                "corr-sandbox-deny",
                ConnectorCapability::Invoke,
            ),
        )
        .unwrap_err();
    assert_eq!(
        err,
        ConnectorHostRuntimeError::SandboxViolation {
            zone_id: "zone.default".to_string(),
            capability: ConnectorCapability::Invoke,
            reason_code: "sandbox.denied.capability.invoke".to_string(),
        }
    );

    let snapshot = runtime.health_snapshot(140);
    assert_eq!(snapshot.phase, ConnectorLifecyclePhase::Failed);
    assert_eq!(snapshot.sandbox_zone_id, "zone.default");
    assert_eq!(
        snapshot
            .last_sandbox_decision
            .as_ref()
            .expect("sandbox decision expected")
            .reason_code,
        "sandbox.denied.capability.invoke"
    );
    assert_eq!(
        snapshot
            .last_failure
            .as_ref()
            .expect("failure expected")
            .class,
        ConnectorFailureClass::Policy
    );
}

#[test]
fn connector_host_runtime_integration_sandbox_allows_scoped_targets() {
    let mut config = ConnectorHostConfig::default();
    config.sandbox.capability_envelope.allowed_capabilities = vec![
        ConnectorCapability::Invoke,
        ConnectorCapability::FilesystemRead,
        ConnectorCapability::NetworkEgress,
    ];
    config.sandbox.capability_envelope.filesystem_read_prefixes =
        vec!["/var/connectors/".to_string()];
    config.sandbox.capability_envelope.network_allow_hosts = vec!["*.svc.local".to_string()];
    let mut runtime = ConnectorHostRuntime::new(config).unwrap();
    runtime.start(1_000).unwrap();
    runtime.observe_usage(1_010, usage_within_budget()).unwrap();

    let fs_envelope = runtime
        .authorize_operation(
            1_020,
            ConnectorOperationRequest::new(
                "connector.fs.read",
                "corr-fs",
                ConnectorCapability::FilesystemRead,
            )
            .with_target("/var/connectors/state.json"),
        )
        .unwrap();
    assert_eq!(fs_envelope.zone_id, "zone.default");
    assert_eq!(fs_envelope.capability, ConnectorCapability::FilesystemRead);
    assert_eq!(
        fs_envelope.target.as_deref(),
        Some("/var/connectors/state.json")
    );

    let net_envelope = runtime
        .authorize_operation(
            1_030,
            ConnectorOperationRequest::new(
                "connector.network.call",
                "corr-net",
                ConnectorCapability::NetworkEgress,
            )
            .with_target("api.svc.local"),
        )
        .unwrap();
    assert_eq!(net_envelope.target.as_deref(), Some("api.svc.local"));

    let decisions = runtime.sandbox_decision_history();
    assert_eq!(decisions.len(), 2);
    assert!(decisions.iter().all(|decision| decision.allowed));
}
