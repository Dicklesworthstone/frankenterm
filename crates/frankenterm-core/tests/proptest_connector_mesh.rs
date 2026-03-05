//! Property-based tests for the connector mesh federation module.

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::connector_host_runtime::{
    ConnectorCapability, ConnectorFailureClass, ConnectorLifecyclePhase,
};

use frankenterm_core::connector_mesh::{
    ConnectorMesh, ConnectorMeshConfig, HostHealth, MeshFailureEvent, MeshHealthSnapshot, MeshHost,
    MeshTelemetrySnapshot, MeshZone, RoutingDecision, RoutingRequest, RoutingStrategy,
};

fn arb_host_health() -> impl Strategy<Value = HostHealth> {
    prop_oneof![
        Just(HostHealth::Healthy),
        Just(HostHealth::Degraded),
        Just(HostHealth::Unreachable),
        Just(HostHealth::Draining),
    ]
}

fn arb_routing_strategy() -> impl Strategy<Value = RoutingStrategy> {
    prop_oneof![
        Just(RoutingStrategy::LeastLoaded),
        Just(RoutingStrategy::ZoneAffinity),
        Just(RoutingStrategy::RoundRobin),
    ]
}

#[allow(dead_code)]
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

fn arb_failure_class() -> impl Strategy<Value = ConnectorFailureClass> {
    prop_oneof![
        Just(ConnectorFailureClass::Auth),
        Just(ConnectorFailureClass::Quota),
        Just(ConnectorFailureClass::Network),
        Just(ConnectorFailureClass::Policy),
        Just(ConnectorFailureClass::Validation),
        Just(ConnectorFailureClass::Timeout),
        Just(ConnectorFailureClass::Unknown),
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

fn arb_zone() -> impl Strategy<Value = MeshZone> {
    (
        prop_oneof![
            Just("us-east".to_string()),
            Just("us-west".to_string()),
            Just("eu-west".to_string()),
            Just("ap-south".to_string()),
        ],
        0u32..1000u32,
        any::<bool>(),
    )
        .prop_map(|(zone_id, priority, active)| {
            let mut z = MeshZone::new(zone_id.clone(), format!("Zone {zone_id}"));
            z.priority = priority;
            z.active = active;
            z
        })
}

fn arb_config() -> impl Strategy<Value = ConnectorMeshConfig> {
    (
        arb_routing_strategy(),
        1000u64..120_000u64,
        10usize..1000usize,
        10usize..1000usize,
        any::<bool>(),
    )
        .prop_map(|(strategy, hb_timeout, max_fail, max_route, cross_zone)| {
            ConnectorMeshConfig {
                default_strategy: strategy,
                heartbeat_timeout_ms: hb_timeout,
                max_failure_history: max_fail,
                max_routing_history: max_route,
                allow_cross_zone_fallback: cross_zone,
            }
        })
}

fn make_host(id: &str, zone: &str) -> MeshHost {
    MeshHost {
        host_id: id.to_string(),
        zone_id: zone.to_string(),
        health: HostHealth::Healthy,
        capabilities: vec![ConnectorCapability::Invoke, ConnectorCapability::ReadState],
        active_connectors: 0,
        max_connectors: 10,
        last_heartbeat_ms: 1000,
        phase: ConnectorLifecyclePhase::Running,
        metadata: BTreeMap::new(),
    }
}

fn build_mesh(n: usize, zone_id: &str) -> ConnectorMesh {
    let mut mesh = ConnectorMesh::new(ConnectorMeshConfig::default());
    mesh.register_zone(MeshZone::new(zone_id, format!("Zone {zone_id}")))
        .unwrap();
    for i in 0..n {
        mesh.register_host(make_host(&format!("host-{i}"), zone_id))
            .unwrap();
    }
    mesh
}

proptest! {
    #[test]
    fn host_health_as_str_never_empty(health in arb_host_health()) {
        let label = health.as_str();
        prop_assert!(!label.is_empty());
    }

    #[test]
    fn host_health_display_matches_as_str(health in arb_host_health()) {
        prop_assert_eq!(format!("{health}"), health.as_str());
    }

    #[test]
    fn host_health_serde_roundtrip(health in arb_host_health()) {
        let json = serde_json::to_string(&health).unwrap();
        let back: HostHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(health, back);
    }

    #[test]
    fn host_health_accepts_work_invariant(health in arb_host_health()) {
        let accepts = health.accepts_work();
        let expected = matches!(health, HostHealth::Healthy | HostHealth::Degraded);
        prop_assert_eq!(accepts, expected);
    }

    #[test]
    fn routing_strategy_serde_roundtrip(strategy in arb_routing_strategy()) {
        let json = serde_json::to_string(&strategy).unwrap();
        let back: RoutingStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(strategy, back);
    }

    #[test]
    fn routing_strategy_as_str_never_empty(strategy in arb_routing_strategy()) {
        prop_assert!(!strategy.as_str().is_empty());
    }

    #[test]
    fn routing_strategy_display_matches_as_str(strategy in arb_routing_strategy()) {
        prop_assert_eq!(format!("{strategy}"), strategy.as_str());
    }

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: ConnectorMeshConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, config);
    }

    #[test]
    fn zone_serde_roundtrip(zone in arb_zone()) {
        let json = serde_json::to_string(&zone).unwrap();
        let back: MeshZone = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, zone);
    }

    #[test]
    fn zone_new_defaults_active(
        id in prop_oneof![Just("a".to_string()), Just("b".to_string())]
    ) {
        let zone = MeshZone::new(id.clone(), format!("Label {id}"));
        prop_assert!(zone.active);
        prop_assert_eq!(zone.priority, 100);
    }

    #[test]
    fn host_can_accept_requires_all_conditions(
        health in arb_host_health(),
        phase in arb_lifecycle_phase(),
        active in 0usize..20usize,
        max_conn in 1usize..20usize,
    ) {
        let host = MeshHost {
            host_id: "h1".to_string(),
            zone_id: "z1".to_string(),
            health,
            capabilities: vec![ConnectorCapability::Invoke],
            active_connectors: active,
            max_connectors: max_conn,
            last_heartbeat_ms: 1000,
            phase,
            metadata: BTreeMap::new(),
        };
        let can = host.can_accept();
        let expected = health.accepts_work()
            && active < max_conn
            && matches!(phase, ConnectorLifecyclePhase::Running);
        prop_assert_eq!(can, expected);
    }

    #[test]
    fn host_remaining_capacity_correct(
        active in 0usize..100usize,
        max_conn in 0usize..100usize,
    ) {
        let host = MeshHost {
            host_id: "h".to_string(),
            zone_id: "z".to_string(),
            health: HostHealth::Healthy,
            capabilities: vec![],
            active_connectors: active,
            max_connectors: max_conn,
            last_heartbeat_ms: 0,
            phase: ConnectorLifecyclePhase::Running,
            metadata: BTreeMap::new(),
        };
        prop_assert_eq!(host.remaining_capacity(), max_conn.saturating_sub(active));
    }

    #[test]
    fn mesh_register_zone_then_host_succeeds(zone in arb_zone()) {
        let mut mesh = ConnectorMesh::new(ConnectorMeshConfig::default());
        mesh.register_zone(zone.clone()).unwrap();
        let host = MeshHost {
            host_id: "test-host".to_string(),
            zone_id: zone.zone_id.clone(),
            health: HostHealth::Healthy,
            capabilities: vec![ConnectorCapability::Invoke],
            active_connectors: 0,
            max_connectors: 10,
            last_heartbeat_ms: 1000,
            phase: ConnectorLifecyclePhase::Running,
            metadata: BTreeMap::new(),
        };
        mesh.register_host(host).unwrap();
        prop_assert_eq!(mesh.hosts().len(), 1);
        prop_assert_eq!(mesh.zones().len(), 1);
    }

    #[test]
    fn mesh_duplicate_zone_fails(zone in arb_zone()) {
        let mut mesh = ConnectorMesh::new(ConnectorMeshConfig::default());
        mesh.register_zone(zone.clone()).unwrap();
        prop_assert!(mesh.register_zone(zone).is_err());
    }

    #[test]
    fn mesh_routing_succeeds_with_eligible_hosts(
        num_hosts in 1usize..10usize,
        strategy in arb_routing_strategy(),
    ) {
        let mut mesh = build_mesh(num_hosts, "z1");
        let request = RoutingRequest {
            connector_id: "conn-1".to_string(),
            required_capabilities: vec![ConnectorCapability::Invoke],
            preferred_zone: Some("z1".to_string()),
            strategy: Some(strategy),
        };
        let decision = mesh.route(&request, 2000).unwrap();
        prop_assert!(!decision.host_id.is_empty());
        prop_assert_eq!(decision.zone_id, "z1");
        prop_assert_eq!(decision.connector_id, "conn-1");
    }

    #[test]
    fn mesh_routing_fails_with_no_hosts(strategy in arb_routing_strategy()) {
        let mut mesh = ConnectorMesh::new(ConnectorMeshConfig::default());
        mesh.register_zone(MeshZone::new("z1", "Zone z1")).unwrap();
        let request = RoutingRequest {
            connector_id: "conn-1".to_string(),
            required_capabilities: vec![],
            preferred_zone: None,
            strategy: Some(strategy),
        };
        prop_assert!(mesh.route(&request, 1000).is_err());
    }

    #[test]
    fn mesh_routing_increments_host_load(num_routes in 1usize..10usize) {
        let mut mesh = build_mesh(1, "z1");
        for i in 0..num_routes {
            let request = RoutingRequest {
                connector_id: format!("conn-{i}"),
                required_capabilities: vec![ConnectorCapability::Invoke],
                preferred_zone: None,
                strategy: None,
            };
            mesh.route(&request, 2000 + i as u64 * 100).unwrap();
        }
        let host = mesh.get_host("host-0").unwrap();
        prop_assert_eq!(host.active_connectors, num_routes);
    }

    #[test]
    fn mesh_routing_history_bounded(
        max_history in 5usize..20usize,
        num_routes in 1usize..50usize,
    ) {
        let config = ConnectorMeshConfig {
            max_routing_history: max_history,
            ..Default::default()
        };
        let mut mesh = ConnectorMesh::new(config);
        mesh.register_zone(MeshZone::new("z1", "Zone z1")).unwrap();
        mesh.register_host(MeshHost {
            host_id: "h1".to_string(),
            zone_id: "z1".to_string(),
            health: HostHealth::Healthy,
            capabilities: vec![ConnectorCapability::Invoke],
            active_connectors: 0,
            max_connectors: 1000,
            last_heartbeat_ms: 1000,
            phase: ConnectorLifecyclePhase::Running,
            metadata: BTreeMap::new(),
        }).unwrap();
        for i in 0..num_routes {
            let request = RoutingRequest {
                connector_id: format!("c-{i}"),
                required_capabilities: vec![ConnectorCapability::Invoke],
                preferred_zone: None,
                strategy: None,
            };
            let _ = mesh.route(&request, 2000 + i as u64 * 10);
        }
        prop_assert!(mesh.routing_history().len() <= max_history);
    }

    #[test]
    fn mesh_update_health_changes_state(new_health in arb_host_health()) {
        let mut mesh = build_mesh(1, "z1");
        mesh.update_health("host-0", new_health).unwrap();
        let host = mesh.get_host("host-0").unwrap();
        prop_assert_eq!(host.health, new_health);
    }

    #[test]
    fn mesh_heartbeat_timeout_marks_unreachable(hb_timeout_ms in 1000u64..30_000u64) {
        let config = ConnectorMeshConfig {
            heartbeat_timeout_ms: hb_timeout_ms,
            ..Default::default()
        };
        let mut mesh = ConnectorMesh::new(config);
        mesh.register_zone(MeshZone::new("z1", "Zone z1")).unwrap();
        mesh.register_host(MeshHost {
            host_id: "h1".to_string(),
            zone_id: "z1".to_string(),
            health: HostHealth::Healthy,
            capabilities: vec![],
            active_connectors: 0,
            max_connectors: 10,
            last_heartbeat_ms: 1000,
            phase: ConnectorLifecyclePhase::Running,
            metadata: BTreeMap::new(),
        }).unwrap();
        let before = mesh.check_heartbeat_timeouts(1000 + hb_timeout_ms - 1);
        prop_assert!(before.is_empty());
        let after = mesh.check_heartbeat_timeouts(1000 + hb_timeout_ms + 1);
        prop_assert_eq!(after.len(), 1);
    }

    #[test]
    fn mesh_failure_history_bounded(
        max_fail in 5usize..20usize,
        num_failures in 1usize..50usize,
    ) {
        let config = ConnectorMeshConfig {
            max_failure_history: max_fail,
            ..Default::default()
        };
        let mut mesh = ConnectorMesh::new(config);
        for i in 0..num_failures {
            mesh.record_failure(MeshFailureEvent {
                host_id: format!("h-{i}"),
                zone_id: "z1".to_string(),
                failure_class: ConnectorFailureClass::Network,
                description: "test".to_string(),
                timestamp_ms: 1000 + i as u64 * 10,
            });
        }
        prop_assert!(mesh.failure_history().len() <= max_fail);
    }

    #[test]
    fn health_snapshot_counts_consistent(
        num_healthy in 0usize..5usize,
        num_degraded in 0usize..5usize,
        num_unreachable in 0usize..5usize,
    ) {
        let mut mesh = ConnectorMesh::new(ConnectorMeshConfig::default());
        mesh.register_zone(MeshZone::new("z1", "Zone z1")).unwrap();
        for i in 0..num_healthy {
            let mut h = make_host(&format!("healthy-{i}"), "z1");
            h.health = HostHealth::Healthy;
            mesh.register_host(h).unwrap();
        }
        for i in 0..num_degraded {
            let mut h = make_host(&format!("degraded-{i}"), "z1");
            h.health = HostHealth::Degraded;
            mesh.register_host(h).unwrap();
        }
        for i in 0..num_unreachable {
            let mut h = make_host(&format!("unreachable-{i}"), "z1");
            h.health = HostHealth::Unreachable;
            mesh.register_host(h).unwrap();
        }
        let snap = mesh.health_snapshot();
        let total = num_healthy + num_degraded + num_unreachable;
        prop_assert_eq!(snap.total_hosts, total);
        prop_assert_eq!(snap.healthy_hosts, num_healthy);
        prop_assert_eq!(snap.degraded_hosts, num_degraded);
        prop_assert_eq!(snap.unreachable_hosts, num_unreachable);
    }

    #[test]
    fn health_snapshot_serde_roundtrip(
        total in 0usize..100usize,
        healthy in 0usize..100usize,
        degraded in 0usize..100usize,
        unreachable_ct in 0usize..100usize,
        zones in 0usize..10usize,
        capacity in 0usize..1000usize,
        active in 0usize..1000usize,
    ) {
        let snap = MeshHealthSnapshot {
            total_hosts: total,
            healthy_hosts: healthy,
            degraded_hosts: degraded,
            unreachable_hosts: unreachable_ct,
            total_zones: zones,
            total_capacity: capacity,
            total_active: active,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: MeshHealthSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn health_snapshot_utilization_bounded(
        capacity in 1usize..1000usize,
        active in 0usize..1000usize,
    ) {
        let snap = MeshHealthSnapshot {
            total_hosts: 1,
            healthy_hosts: 1,
            degraded_hosts: 0,
            unreachable_hosts: 0,
            total_zones: 1,
            total_capacity: capacity,
            total_active: active,
        };
        let util = snap.utilization();
        prop_assert!(util >= 0.0);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        registered in 0u64..100u64,
        deregistered in 0u64..100u64,
        zones_created in 0u64..100u64,
        routing_req in 0u64..100u64,
        routing_ok in 0u64..100u64,
        routing_fail in 0u64..100u64,
        health_up in 0u64..100u64,
        failures in 0u64..100u64,
        heartbeats in 0u64..100u64,
    ) {
        let snap = MeshTelemetrySnapshot {
            hosts_registered: registered,
            hosts_deregistered: deregistered,
            zones_created,
            routing_requests: routing_req,
            routing_successes: routing_ok,
            routing_failures: routing_fail,
            health_updates: health_up,
            failure_events: failures,
            heartbeats_received: heartbeats,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: MeshTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }

    #[test]
    fn mesh_telemetry_routing_counts(num_routes in 1usize..20usize) {
        let mut mesh = build_mesh(3, "z1");
        for i in 0..num_routes {
            let request = RoutingRequest {
                connector_id: format!("c-{i}"),
                required_capabilities: vec![ConnectorCapability::Invoke],
                preferred_zone: None,
                strategy: None,
            };
            let _ = mesh.route(&request, 2000 + i as u64 * 10);
        }
        let tel = mesh.telemetry().snapshot();
        prop_assert_eq!(tel.routing_requests, num_routes as u64);
        prop_assert_eq!(tel.routing_successes, num_routes as u64);
        prop_assert_eq!(tel.routing_failures, 0);
    }

    #[test]
    fn routing_decision_serde_roundtrip(
        strategy in arb_routing_strategy(),
        ts in 1u64..1_000_000u64,
    ) {
        let decision = RoutingDecision {
            connector_id: "conn-1".to_string(),
            host_id: "host-1".to_string(),
            zone_id: "us-east".to_string(),
            strategy_used: strategy,
            decided_at_ms: ts,
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: RoutingDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decision, back);
    }

    #[test]
    fn failure_event_serde_roundtrip(class in arb_failure_class()) {
        let event = MeshFailureEvent {
            host_id: "h-1".to_string(),
            zone_id: "z-1".to_string(),
            failure_class: class,
            description: "test".to_string(),
            timestamp_ms: 5000,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: MeshFailureEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event, back);
    }
}
