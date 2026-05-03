//! E2E fixture: CapabilityPassport filters mux dispatch end-to-end
//! (ft-yim20 — last open ft-1650n.1 acceptance item before the
//! follow-up beads for doctor output and durable SQLite store).
//!
//! Exercises the full pipeline shipped over slices 1-7 of ft-1650n.1:
//!
//!   RegisterPaneWithPassport (slice 6)
//!     → PassportStore + Declared-only entries
//!       → ProbeRunner promotes a subset to Verified (slice 7)
//!         → GuardedCommand (slice 4) routes ONLY to panes whose
//!           passport has the required class at Verified
//!
//! The fixture builds three panes with distinct capability profiles:
//!
//! - alpha: ToolAvailability("bash") + ToolAvailability("write") at
//!   Verified — most-permissive.
//! - bravo: ToolAvailability("bash") at Verified, NO write.
//! - charlie: everything still at Declared (probes have not run) —
//!   fail-closed: nothing authorized.
//!
//! Then dispatches three GuardedCommand requests targeting each pane
//! with different required_classes, asserting:
//!
//! 1. A bash-only requirement is allowed against alpha and bravo,
//!    denied against charlie (Declared !== Verified, fail-closed).
//! 2. A bash+write requirement is allowed against alpha only,
//!    denied against bravo (no Verified write) and charlie.
//! 3. An empty required_classes list is allowed against any pane
//!    that has a passport (vacuous truth from PreflightChecker;
//!    distinct from MissingPassport).

use std::sync::Arc;
use std::time::Duration;

use frankenterm_core::capability_passport::CapabilityClass;
use frankenterm_core::capability_passport_store::{PassportKey, PassportStore};
use frankenterm_core::capability_preflight::PreflightOutcome;
use frankenterm_core::capability_probe::{ProbeRunner, ToolAvailabilityProbe};
use frankenterm_core::command_transport::{
    CommandContext, CommandKind, CommandRequest, CommandScope,
};
use frankenterm_core::headless_mux_server::{
    HeadlessMuxServer, RemoteRequest, RemoteResponse, ServerConfig,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleState, MuxPaneLifecycleState,
};

// ── Test fixture helpers ─────────────────────────────────────────────────

fn pane_identity(local_id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(
        LifecycleEntityKind::Pane,
        "ws-yim20",
        "local",
        local_id,
        1,
    )
}

fn make_command(local_id: u64) -> CommandRequest {
    // Targets the registered pane so the router can find it.
    CommandRequest {
        command_id: format!("yim20-cmd-{local_id}"),
        scope: CommandScope::pane(pane_identity(local_id)),
        command: CommandKind::SendInput {
            text: "noop\n".to_string(),
            paste_mode: false,
            append_newline: false,
        },
        context: CommandContext {
            timestamp_ms: 0,
            component: "yim20-test".into(),
            correlation_id: format!("yim20-corr-{local_id}"),
            caller_identity: "yim20-actor".into(),
            reason: None,
            policy_trace: None,
        },
        dry_run: false,
    }
}

fn build_swarm() -> (HeadlessMuxServer, Arc<PassportStore>) {
    let store = Arc::new(PassportStore::new());

    // Generous freshness window so probe-promoted timestamps don't
    // age out during the test.
    let mut server = HeadlessMuxServer::new(ServerConfig::default())
        .with_passport_store_and_freshness(store.clone(), 60 * 60 * 1_000);

    // Register three panes with Declared-only passports via the
    // slice-6 endpoint. Each declares the same set of bash + write
    // tools so the upstream Declared shape is identical — the only
    // post-probe difference is which classes survive promotion.
    for (local_id, agent) in [(1u64, "alpha"), (2, "bravo"), (3, "charlie")] {
        let resp = server.handle_request(RemoteRequest::RegisterPaneWithPassport {
            identity: pane_identity(local_id),
            state: LifecycleState::Pane(MuxPaneLifecycleState::Running),
            agent_id: agent.into(),
            declared_capabilities: vec![
                CapabilityClass::ToolAvailability("bash".into()),
                CapabilityClass::ToolAvailability("write".into()),
            ],
        });
        assert!(matches!(resp, RemoteResponse::PaneRegistered { .. }));
    }

    // Run probes to PROMOTE Declared → Verified per fixture spec:
    // - alpha: bash + write available → both Verified.
    // - bravo: only bash available → bash Verified, write stays Declared.
    // - charlie: no probes run → everything stays Declared.
    let mut runner_alpha = ProbeRunner::new();
    runner_alpha
        .add_probe(ToolAvailabilityProbe::new(
            "bash",
            vec!["bash".into(), "write".into()],
        ))
        .add_probe(ToolAvailabilityProbe::new(
            "write",
            vec!["bash".into(), "write".into()],
        ));
    let mut alpha_pass = store
        .get(&PassportKey::pane("alpha", 1))
        .expect("alpha registered");
    runner_alpha.run(&mut alpha_pass, Duration::from_millis(100));
    store.insert(alpha_pass);

    let mut runner_bravo = ProbeRunner::new();
    runner_bravo
        .add_probe(ToolAvailabilityProbe::new("bash", vec!["bash".into()]))
        .add_probe(ToolAvailabilityProbe::new(
            "write",
            vec!["bash".into()],
        ));
    let mut bravo_pass = store
        .get(&PassportKey::pane("bravo", 2))
        .expect("bravo registered");
    runner_bravo.run(&mut bravo_pass, Duration::from_millis(100));
    store.insert(bravo_pass);

    // charlie's passport stays Declared-only — operator hasn't probed.

    (server, store)
}

// ── E2E assertions ───────────────────────────────────────────────────────

/// A bash-only requirement is allowed against alpha (bash Verified)
/// and bravo (bash Verified), denied against charlie (Declared-only —
/// fail-closed).
#[test]
fn bash_only_dispatch_routes_to_alpha_and_bravo_denies_charlie_yim20() {
    let (mut server, _store) = build_swarm();
    let required = vec![CapabilityClass::ToolAvailability("bash".into())];

    // alpha allowed.
    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("alpha", 1),
        required_classes: required.clone(),
        request: Box::new(make_command(1)),
    });
    assert_not_preflight_denied(&resp, "alpha bash");

    // bravo allowed.
    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("bravo", 2),
        required_classes: required.clone(),
        request: Box::new(make_command(2)),
    });
    assert_not_preflight_denied(&resp, "bravo bash");

    // charlie denied (Declared-only — fail-closed).
    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("charlie", 3),
        required_classes: required,
        request: Box::new(make_command(3)),
    });
    assert_preflight_denied(&resp, "missing_capabilities", "charlie bash");
}

/// A bash+write requirement is allowed against alpha only.
/// bravo denied (write still Declared after probe); charlie denied
/// (everything Declared).
#[test]
fn bash_and_write_dispatch_routes_only_to_alpha_yim20() {
    let (mut server, _store) = build_swarm();
    let required = vec![
        CapabilityClass::ToolAvailability("bash".into()),
        CapabilityClass::ToolAvailability("write".into()),
    ];

    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("alpha", 1),
        required_classes: required.clone(),
        request: Box::new(make_command(1)),
    });
    assert_not_preflight_denied(&resp, "alpha bash+write");

    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("bravo", 2),
        required_classes: required.clone(),
        request: Box::new(make_command(2)),
    });
    assert_preflight_denied(&resp, "missing_capabilities", "bravo bash+write");

    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("charlie", 3),
        required_classes: required,
        request: Box::new(make_command(3)),
    });
    assert_preflight_denied(&resp, "missing_capabilities", "charlie bash+write");
}

/// PreflightOutcome::Allowed is correctly distinguished from
/// MissingPassport: a request against a registered actor with an
/// EMPTY required_classes list is allowed (vacuous truth — the
/// passport exists, no capabilities required); against an unknown
/// actor it's MissingPassport.
#[test]
fn empty_requirements_allowed_for_registered_actor_missing_for_unknown_yim20() {
    let (mut server, _store) = build_swarm();

    // Registered actor with empty requirements → router runs.
    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("alpha", 1),
        required_classes: vec![],
        request: Box::new(make_command(1)),
    });
    assert_not_preflight_denied(&resp, "alpha empty");

    // Unknown actor with empty requirements → MissingPassport.
    let resp = server.handle_request(RemoteRequest::GuardedCommand {
        actor: PassportKey::pane("ghost", 999),
        required_classes: vec![],
        request: Box::new(make_command(1)),
    });
    assert_preflight_denied(&resp, "missing_passport", "ghost empty");
}

/// Preflight RPC (read-only, slice 3) reports the actual outcome
/// shape end-to-end for an external client to consult before
/// committing to a GuardedCommand. Mirrors the deny path's diagnostic
/// shape: MissingCapabilities carries `present_at` so the operator
/// can see that bravo's write is at Declared (not absent).
#[test]
fn preflight_rpc_reports_present_at_for_partially_declared_passport_yim20() {
    let (mut server, _store) = build_swarm();
    let resp = server.handle_request(RemoteRequest::PassportPreflight {
        key: PassportKey::pane("bravo", 2),
        required_classes: vec![CapabilityClass::ToolAvailability("write".into())],
    });
    let outcome = match resp {
        RemoteResponse::PreflightOutcome { outcome } => outcome,
        other => panic!("expected PreflightOutcome, got {other:?}"),
    };
    let PreflightOutcome::MissingCapabilities { unmet, present_at } = outcome else {
        panic!("expected MissingCapabilities for bravo write");
    };
    assert_eq!(unmet, vec![CapabilityClass::ToolAvailability("write".into())]);
    // bravo declared write but the probe couldn't verify it →
    // present_at carries the Declared verification, distinguishing
    // "never declared" from "declared but not verified".
    use frankenterm_core::capability_passport::CapabilityVerification;
    assert_eq!(present_at.len(), 1);
    assert_eq!(present_at[0].1, Some(CapabilityVerification::Declared));
}

// ── Assertion helpers ────────────────────────────────────────────────────

fn assert_preflight_denied(resp: &RemoteResponse, expected_label: &str, ctx: &str) {
    match resp {
        RemoteResponse::Error { code, message } => {
            assert_eq!(
                code, "preflight_denied",
                "[{ctx}] expected preflight_denied, got {code}: {message}"
            );
            assert!(
                message.contains(expected_label),
                "[{ctx}] expected message to contain {expected_label:?}, got: {message}"
            );
        }
        other => panic!("[{ctx}] expected Error preflight_denied, got {other:?}"),
    }
}

fn assert_not_preflight_denied(resp: &RemoteResponse, ctx: &str) {
    match resp {
        RemoteResponse::Error { code, .. } => {
            assert_ne!(
                code, "preflight_denied",
                "[{ctx}] preflight should have allowed but saw preflight_denied"
            );
            // Other error codes (command_failed, etc) are acceptable
            // — the router was reached after preflight allowed.
        }
        RemoteResponse::CommandResult { .. } => {
            // Acceptable: router ran (whether delivered or not).
        }
        other => panic!("[{ctx}] unexpected response: {other:?}"),
    }
}
