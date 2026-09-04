//! Conformance / wiring-attestation guards for the connector bridges under the
//! W4 "Dead-Wire Closure + Wiring Attestation Gate" epic (ft-7h5da.5):
//!   - ft-7h5da.5.9  inbound bridge
//!   - ft-7h5da.5.10 outbound bridge + governor
//!   - ft-7h5da.5.11 lifecycle/mesh boundary
//!
//! These assert two invariants by inspecting the PRODUCTION SOURCE, so a
//! refactor that silently un-wires a bridge fails CI (an isolated behavioral
//! unit test in the bridge module would keep passing while the bridge went
//! dark again):
//!   1. Each connector bridge keeps a real PRODUCTION caller / boundary.
//!   2. The outbound dispatch path keeps CONSULTING the connector governor and
//!      FAILS CLOSED (only `GovernorVerdict::Allow` may proceed; sandbox is
//!      enforced by default).
//!
//! Source-level (not behavioral) on purpose: the gap these beads describe is
//! that the bridges' real logic already works but had no production wiring;
//! behavioral coverage already lives next to each bridge. These guards pin the
//! WIRE itself.

use std::{
    error::Error,
    io,
    path::{Path, PathBuf},
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// Read a `frankenterm-core` source file relative to the crate manifest.
fn core_src(file: &str) -> TestResult<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path)
        .map_err(|err| io::Error::new(err.kind(), format!("read {}: {err}", path.display())).into())
}

fn crate_test_src(file: &str) -> TestResult<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(file);
    std::fs::read_to_string(&path)
        .map_err(|err| io::Error::new(err.kind(), format!("read {}: {err}", path.display())).into())
}

fn workspace_src(relative_path: &str) -> TestResult<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative_path);
    std::fs::read_to_string(&path)
        .map_err(|err| io::Error::new(err.kind(), format!("read {}: {err}", path.display())).into())
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

fn updating_goldens() -> bool {
    std::env::var_os("UPDATE_GOLDENS").is_some()
}

fn assert_golden_json(relative_path: &str, actual: &serde_json::Value) -> TestResult {
    let path = golden_dir().join(relative_path);
    let actual_text = serde_json::to_string_pretty(actual)? + "\n";

    if updating_goldens() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, actual_text)?;
        eprintln!("[GOLDEN UPDATED] {}", path.display());
        return Ok(());
    }

    let expected = std::fs::read_to_string(&path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "golden missing: {}\n\
             cause: {err}\n\
             run: UPDATE_GOLDENS=1 cargo test -p frankenterm-core \
             --test connector_bridge_wiring_conformance\n\
             then: git diff {}",
                path.display(),
                path.display()
            ),
        )
    })?;

    if actual_text != expected {
        let actual_path = path.with_extension("actual.json");
        std::fs::write(&actual_path, actual_text.as_bytes()).ok();
        assert_eq!(
            actual_text,
            expected,
            "GOLDEN MISMATCH: {relative_path}\n\
             diff {} {}\n\
             Regenerating this golden means the connector bridge production \
             wiring contract changed.",
            path.display(),
            actual_path.display()
        );
    }
    Ok(())
}

fn contains_all(src: &str, needles: &[&str]) -> bool {
    needles.iter().all(|needle| src.contains(needle))
}

fn connector_bridge_contract_matrix() -> TestResult<serde_json::Value> {
    let runtime = core_src("runtime.rs")?;
    let inbound = core_src("connector_inbound_bridge.rs")?;
    let outbound = core_src("connector_outbound_bridge.rs")?;
    let policy = core_src("policy.rs")?;
    let conformance = crate_test_src("connector_bridge_wiring_conformance.rs")?;
    let e2e = workspace_src("tests/e2e/test_connector_bridge_wiring_conformance.sh")?;

    Ok(serde_json::json!([
        {
            "bead": "ft-7h5da.5.9",
            "bridge": "inbound",
            "contract": "runtime-owned ingress routes ConnectorSignal into EventBus",
            "production_caller_wired": contains_all(
                &runtime,
                &[
                    "ConnectorInboundBridge::new(",
                    "pub fn route_connector_signal",
                    "route_connector_signal_through_bridge",
                    "guard.route_signal(signal)",
                ],
            ),
            "fail_closed": contains_all(
                &runtime,
                &[
                    "ConnectorBridgeError::BridgeUnavailable",
                    "runtime was not configured with an event bus",
                    "connector inbound bridge lock poisoned",
                ],
            ) && contains_all(
                &inbound,
                &[
                    "ConnectorBridgeError::UnknownKindRejected",
                    "ConnectorBridgeError::PrivacyRejected",
                    "ConnectorBridgeError::PrivacyQuarantined",
                    "ConnectorBridgeError::ClassificationFailed",
                ],
            ),
            "no_silent_drop": contains_all(
                &inbound,
                &[
                    "let delivered = self.event_bus.publish(event);",
                    "delivered_count: delivered",
                    "events_published",
                ],
            ),
        },
        {
            "bead": "ft-7h5da.5.10",
            "bridge": "outbound",
            "contract": "runtime EventBus feeds outbound admission; unavailable delivery fails closed without success or retry",
            "production_caller_wired": contains_all(
                &runtime,
                &[
                    "ConnectorOutboundBridge::new(",
                    "spawn_connector_outbound_task",
                    "OutboundEvent::from_runtime_event(",
                    "bridge.process_event(&outbound_event)",
                    "bridge.drain_actions()",
                    "dispatch_connector_outbound_action",
                ],
            ),
            "fail_closed": contains_all(
                &outbound,
                &[
                    "controller.allow_operation()",
                    "governor.evaluate(&action, now_ms)",
                    "!matches!(&governor_decision.verdict, GovernorVerdict::Allow)",
                    "actions_blocked_governor",
                    "actions_blocked_reliability",
                    "connector_admission_enforced",
                ],
            ) && contains_all(
                &runtime,
                &[
                    "ConnectorOutboundDeliveryError::TransportUnavailable",
                    "record_action_failure(action, error.code(), ConnectorErrorKind::Permanent, now_ms)",
                    "delivered = false",
                ],
            ),
            "no_silent_drop": contains_all(
                &runtime,
                &[
                    "RecvError::Lagged",
                    "missed_count",
                    "connector outbound bridge lagged on event bus",
                    "connector outbound bridge lock poisoned; dropping runtime event",
                    "connector outbound bridge rejected runtime event",
                ],
            ),
        },
        {
            "bead": "ft-7h5da.5.11",
            "bridge": "lifecycle_mesh",
            "contract": "PolicyEngine exposes lifecycle/mesh admission models; runtime refuses delivery without transport",
            "production_caller_wired": contains_all(
                &policy,
                &[
                    "pub fn run_connector_lifecycle_intent",
                    "self.lifecycle_manager_mut().execute(intent, now_ms)",
                    "connector lifecycle intent executed via production boundary",
                    "pub fn route_connector_operation_through_mesh",
                    ".route(&routing_request, now_ms)",
                ],
            ) && contains_all(
                &runtime,
                &[
                    "ConnectorOutboundDeliveryError::TransportUnavailable",
                    "dispatch_connector_outbound_action",
                ],
            ),
            "fail_closed": contains_all(
                &policy,
                &[
                    "kill_switch().is_emergency()",
                    "connector lifecycle denied: emergency kill switch active",
                    "ConnectorOperationDispatchError::Denied",
                    "ConnectorOperationDispatchError::from_mesh_error",
                    "ConnectorOperationDispatchError::from_host_runtime_error",
                    "return Err(",
                    "telemetry is not advanced on a denial",
                ],
            ) && contains_all(
                &runtime,
                &[
                    "ConnectorOutboundDeliveryError::TransportUnavailable",
                    "delivered = false",
                    "ConnectorErrorKind::Permanent",
                ],
            ),
            "no_silent_drop": contains_all(
                &policy,
                &[
                    "connector lifecycle intent executed via production boundary",
                    "connector lifecycle intent failed at production boundary",
                    "connector operation routed through production mesh boundary",
                    "record_failure(crate::connector_mesh::MeshFailureEvent",
                    ".release_connector(&routing_decision.host_id)",
                    "Err(err.to_string())",
                ],
            ) && contains_all(
                &runtime,
                &[
                    "connector outbound action was not dispatched",
                    "record_action_failure(action, error.code(), ConnectorErrorKind::Permanent, now_ms)",
                ],
            ),
        },
        {
            "bead": "ft-7h5da.5.13",
            "bridge": "conformance",
            "contract": "golden and zero-RCH e2e harness preserve connector bridge wiring contracts",
            "production_caller_wired": contains_all(
                &conformance,
                &[
                    "connector_bridge_contract_matrix_matches_golden",
                    "inbound_bridge_has_runtime_production_ingress",
                    "outbound_bridge_has_runtime_production_dispatch",
                    "lifecycle_routes_through_policy_engine_boundary",
                ],
            ) && contains_all(
                &e2e,
                &[
                    "ft-7h5da.5.9",
                    "ft-7h5da.5.10",
                    "ft-7h5da.5.11",
                    "ft-7h5da.5.13",
                ],
            ),
            "fail_closed": contains_all(
                &conformance,
                &[
                    "assert_contract_matrix_all_true",
                    "outbound_dispatch_consults_governor_and_fails_closed",
                    "fail_closed",
                    "actions_blocked_governor",
                ],
            ) && contains_all(
                &e2e,
                &[
                    "forbidden Cargo/RCH invocation",
                    "governor fail-closed",
                    "exit 1",
                ],
            ),
            "no_silent_drop": contains_all(
                &conformance,
                &[
                    "no_silent_drop",
                    "assert_golden_json",
                    "conformance_matrix.json",
                ],
            ) && contains_all(
                &e2e,
                &[
                    "golden matrix",
                    "all_checks_passed",
                    "test_connector_bridge_wiring_conformance.sh",
                ],
            ),
        },
    ]))
}

fn assert_contract_matrix_all_true(matrix: &serde_json::Value) -> TestResult {
    let cases = matrix.as_array().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "connector bridge conformance matrix is not an array",
        )
    })?;
    assert_eq!(
        cases.len(),
        4,
        "matrix must cover inbound, outbound, lifecycle/mesh, and conformance harness"
    );
    for case in cases {
        for field in ["production_caller_wired", "fail_closed", "no_silent_drop"] {
            assert_eq!(
                case.get(field).and_then(serde_json::Value::as_bool),
                Some(true),
                "connector bridge contract field {field} must be true for case {case:#}"
            );
        }
    }
    Ok(())
}

/// ft-7h5da.5.9: `ConnectorInboundBridge` must keep a production ingress in the
/// runtime — constructed by the runtime and reachable through a public route
/// method that flows through the owned bridge — not be confined to the
/// connector module + tests.
#[test]
fn inbound_bridge_has_runtime_production_ingress() -> TestResult {
    let src = core_src("runtime.rs")?;
    assert!(
        src.contains("ConnectorInboundBridge::new("),
        "runtime.rs must construct ConnectorInboundBridge (ft-7h5da.5.9 ingress)"
    );
    assert!(
        src.contains("pub fn route_connector_signal"),
        "runtime.rs must expose the route_connector_signal production ingress (ft-7h5da.5.9)"
    );
    assert!(
        src.contains("route_connector_signal_through_bridge"),
        "route_connector_signal must route through the owned inbound bridge (ft-7h5da.5.9)"
    );
    Ok(())
}

/// ft-7h5da.5.10: `ConnectorOutboundBridge` must be constructed by the runtime
/// AND fed by a production dispatch path (runtime events -> `OutboundEvent` ->
/// bridge), not only exercised from tests.
#[test]
fn outbound_bridge_has_runtime_production_dispatch() -> TestResult {
    let src = core_src("runtime.rs")?;
    assert!(
        src.contains("ConnectorOutboundBridge::new("),
        "runtime.rs must construct ConnectorOutboundBridge (ft-7h5da.5.10)"
    );
    assert!(
        src.contains("OutboundEvent::from_runtime_event("),
        "runtime.rs must convert runtime events into OutboundEvent for the outbound bridge \
         (ft-7h5da.5.10 production dispatch)"
    );
    assert!(
        src.contains("bridge.drain_actions()"),
        "runtime.rs must drain outbound ConnectorAction values after process_event \
         (ft-7h5da.5.10)"
    );
    assert!(
        src.contains("ConnectorOutboundDeliveryError::TransportUnavailable")
            && src.contains("ConnectorErrorKind::Permanent")
            && !src.contains("record_action_success(action, now_ms)"),
        "runtime.rs must refuse unavailable transport without recording delivery success"
    );
    Ok(())
}

/// ft-7h5da.5.10 governor: the outbound dispatch path must CONSULT the connector
/// governor and FAIL CLOSED. Only `GovernorVerdict::Allow` may proceed; every
/// other verdict blocks and bumps the `actions_blocked_governor` counter; and
/// sandbox enforcement carries a fail-closed default.
#[test]
fn outbound_dispatch_consults_governor_and_fails_closed() -> TestResult {
    let src = core_src("connector_outbound_bridge.rs")?;
    assert!(
        src.contains("connector_governor::GovernorVerdict"),
        "outbound bridge must consult connector_governor::GovernorVerdict (ft-7h5da.5.10 governor)"
    );
    assert!(
        src.contains(".evaluate(&action"),
        "outbound dispatch must call the governor's evaluate() on the pending action \
         (ft-7h5da.5.10)"
    );
    // Fail-closed: anything that is not `Allow` is blocked.
    assert!(
        src.contains("!matches!(&governor_decision.verdict, GovernorVerdict::Allow)"),
        "outbound dispatch must fail closed: only GovernorVerdict::Allow may proceed \
         (ft-7h5da.5.10)"
    );
    assert!(
        src.contains("actions_blocked_governor"),
        "governor denials must increment the actions_blocked_governor telemetry (ft-7h5da.5.10)"
    );
    assert!(
        src.contains("fn default_enforce_sandbox"),
        "sandbox enforcement must have a fail-closed default (ft-7h5da.5.10)"
    );
    Ok(())
}

#[test]
fn connector_bridge_contract_matrix_matches_golden() -> TestResult {
    let matrix = connector_bridge_contract_matrix()?;
    assert_contract_matrix_all_true(&matrix)?;
    assert_golden_json("connector_bridge_wiring/conformance_matrix.json", &matrix)
}

/// ft-7h5da.5.11: connector lifecycle mutations must route through the single
/// gated `PolicyEngine` production boundary (which drives the owned manager so
/// `op_counter` telemetry reflects real operations), not via a direct
/// `lifecycle_manager_mut().execute()` from operator surfaces.
#[test]
fn lifecycle_routes_through_policy_engine_boundary() -> TestResult {
    let src = core_src("policy.rs")?;
    assert!(
        src.contains("pub fn run_connector_lifecycle_intent"),
        "policy.rs must expose the run_connector_lifecycle_intent production boundary \
         (ft-7h5da.5.11)"
    );
    assert!(
        src.contains("self.lifecycle_manager_mut().execute("),
        "the lifecycle boundary must drive the owned ConnectorLifecycleManager (ft-7h5da.5.11)"
    );
    assert!(
        src.contains("kill_switch().is_emergency()")
            && src.contains("connector lifecycle denied: emergency kill switch active"),
        "the lifecycle boundary must fail closed before mutating the manager when the \
         emergency kill switch is active (ft-7h5da.5.11)"
    );
    assert!(
        src.contains("connector lifecycle intent executed via production boundary")
            && src.contains("connector lifecycle intent failed at production boundary"),
        "the lifecycle boundary must log both success and failure outcomes instead of \
         silently dropping lifecycle results (ft-7h5da.5.11)"
    );
    assert!(
        src.contains("pub fn route_connector_operation_through_mesh")
            && src.contains(".route(&routing_request, now_ms)")
            && src.contains("ConnectorOperationDispatchError::from_mesh_error")
            && src.contains("ConnectorOperationDispatchError::from_host_runtime_error"),
        "policy.rs must expose a fail-closed mesh route boundary that drives ConnectorMesh and \
         ConnectorHostRuntime for connector actions (ft-7h5da.5.11)"
    );
    let runtime = core_src("runtime.rs")?;
    assert!(
        runtime.contains("ConnectorOutboundDeliveryError::TransportUnavailable")
            && runtime.contains("ConnectorErrorKind::Permanent")
            && !runtime.contains(".route_connector_operation_through_mesh(")
            && !runtime.contains("record_action_success(action, now_ms)"),
        "runtime outbound delivery must fail closed; mesh admission is not transport execution"
    );
    Ok(())
}
