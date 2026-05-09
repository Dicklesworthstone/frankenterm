//! E2E: full handoff-capsule pipeline through the real RemoteRequest
//! API (ft-1650n.5 slice 5 — final integration).
//!
//! Threads together every slice that ft-1650n.1 (CapabilityPassport)
//! and ft-1650n.5 (HandoffCapsule) shipped:
//!
//! 1. RegisterPaneWithPassport (ft-1650n.1 slice 6) — actor pane
//!    bootstraps with Declared-only passport.
//! 2. ProbeRunner (ft-1650n.1 slice 7) — promotes the actor's
//!    `accept_passport_excerpt` SafetyConstraint from Declared →
//!    Verified so the actor can receive PassportExcerpt sections.
//! 3. ApplyHandoffCapsule (ft-1650n.5 slice 4) — operator sends a
//!    capsule containing a PassportExcerpt for an inherited pane
//!    that should be allowed to dispatch.
//! 4. PassportPreflight (ft-1650n.1 slice 3) — confirms the
//!    inherited passport now satisfies a dispatch requirement,
//!    closing the "operator handoff actually unlocked downstream
//!    work" loop.
//!
//! Plus: tampered-capsule path proves integrity short-circuits
//! before the server's PassportStore is touched.

use std::sync::Arc;
use std::time::Duration;

use frankenterm_core::capability_passport::{
    CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
};
use frankenterm_core::capability_passport_store::{PassportKey, PassportStore};
use frankenterm_core::capability_preflight::PreflightOutcome;
use frankenterm_core::capability_probe::ProbeRunner;
use frankenterm_core::handoff_capsule::{CapsuleEndpoint, CapsuleSection, HandoffCapsule};
use frankenterm_core::headless_mux_server::{
    HeadlessMuxServer, PassportExcerptDisposition, RemoteRequest, RemoteResponse, ServerConfig,
};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleState, MuxPaneLifecycleState,
};

// ── Fixture helpers ────────────────────────────────────────────────────

fn actor_identity() -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "ws-pipeline", "local", 11, 1)
}

fn actor_key() -> PassportKey {
    PassportKey::pane("actor-agent", 11)
}

fn inherited_pane_key() -> PassportKey {
    PassportKey::pane("inherited-agent", 99)
}

/// Promote the actor's `accept_passport_excerpt` SafetyConstraint
/// from Declared (which slice-6 RegisterPaneWithPassport issued)
/// to Verified via the slice-7 ProbeRunner. Mirrors how a real
/// operator probe pass would unlock the actor for handoff
/// inheritance.
fn probe_promote_actor(store: &Arc<PassportStore>) {
    let mut runner = ProbeRunner::new();
    runner.add_probe(probe_for_safety("accept_passport_excerpt"));
    let mut passport = store.get(&actor_key()).expect("actor passport present");
    runner.run(&mut passport, Duration::from_millis(100));
    store.insert(passport);
}

/// Custom CapabilityProbe that verifies a SafetyConstraint by name —
/// the slice-7 ToolAvailabilityProbe targets ToolAvailability
/// classes only, so this fixture supplies a tiny adapter.
struct SafetyConstraintProbe {
    name: String,
}
impl frankenterm_core::capability_probe::CapabilityProbe for SafetyConstraintProbe {
    fn class(&self) -> CapabilityClass {
        CapabilityClass::SafetyConstraint(self.name.clone())
    }
    fn probe(
        &self,
        _deadline: std::time::Instant,
    ) -> frankenterm_core::capability_probe::ProbeOutcome {
        frankenterm_core::capability_probe::ProbeOutcome::Verified(RedactedProof::from_value(
            self.name.as_bytes(),
        ))
    }
}
fn probe_for_safety(name: &str) -> SafetyConstraintProbe {
    SafetyConstraintProbe {
        name: name.to_string(),
    }
}

fn inherited_passport(generation: u64) -> CapabilityPassport {
    CapabilityPassport {
        agent_id: "inherited-agent".into(),
        pane_id: Some(99),
        capabilities: vec![CapabilityEntry {
            class: CapabilityClass::ToolAvailability("bash".into()),
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(900_000),
            proof: RedactedProof::from_value(b"bash-handshake"),
        }],
        generation,
        signed_at_ms: 900_000,
    }
}

fn build_capsule_with_excerpt(passport: CapabilityPassport) -> HandoffCapsule {
    HandoffCapsule::build(
        CapsuleEndpoint {
            agent_id: "source-agent".into(),
            pane_id: Some(1),
            label: Some("shift-A".into()),
        },
        CapsuleEndpoint {
            agent_id: "actor-agent".into(),
            pane_id: Some(11),
            label: Some("shift-B".into()),
        },
        vec![
            CapsuleSection::ContextSummary {
                text: "operator handoff context".into(),
            },
            CapsuleSection::PassportExcerpt { passport },
        ],
        1_500_000,
    )
}

fn build_server() -> (HeadlessMuxServer, Arc<PassportStore>) {
    let store = Arc::new(PassportStore::new());
    let server = HeadlessMuxServer::new(ServerConfig::default())
        .with_passport_store_and_freshness(store.clone(), 60 * 60 * 1_000);
    (server, store)
}

// ── Slice 5 E2E assertions ─────────────────────────────────────────────

/// End-to-end pipeline:
/// 1. Register the actor pane with declared accept_passport_excerpt.
/// 2. Probe-promote the SafetyConstraint to Verified.
/// 3. ApplyHandoffCapsule — server inserts the inherited passport.
/// 4. PassportPreflight against the inherited key — Allowed (proves
///    the inheritance unlocked downstream dispatch).
#[test]
fn full_pipeline_register_promote_apply_then_preflight_allows_inherited_dispatch_ft_1650n_5() {
    let (mut server, store) = build_server();

    // Step 1: register actor pane with the SafetyConstraint declared.
    let resp = server.handle_request(RemoteRequest::RegisterPaneWithPassport {
        identity: actor_identity(),
        state: LifecycleState::Pane(MuxPaneLifecycleState::Running),
        agent_id: "actor-agent".into(),
        declared_capabilities: vec![CapabilityClass::SafetyConstraint(
            "accept_passport_excerpt".into(),
        )],
    });
    assert!(matches!(resp, RemoteResponse::PaneRegistered { .. }));

    // Step 2: probe-promote actor's accept_passport_excerpt to Verified.
    probe_promote_actor(&store);
    let actor_passport = store.get(&actor_key()).expect("actor present");
    assert_eq!(
        actor_passport.capabilities[0].verification,
        CapabilityVerification::Verified,
        "probe should have promoted Declared → Verified"
    );

    // Step 3: send ApplyHandoffCapsule — server applies the
    // PassportExcerpt into its own store.
    let capsule = build_capsule_with_excerpt(inherited_passport(1));
    let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
        actor: actor_key(),
        capsule: Box::new(capsule),
    });
    let RemoteResponse::HandoffApplied { summary } = resp else {
        panic!("expected HandoffApplied, got {resp:?}");
    };
    assert_eq!(summary.accepted_indices, vec![0, 1]);
    assert!(summary.skipped.is_empty());
    // 1 PassportExcerpt section applied; 1 ContextSummary deferred.
    assert_eq!(summary.passport_excerpt_applies.len(), 1);
    assert_eq!(
        summary.passport_excerpt_applies[0].disposition,
        PassportExcerptDisposition::Applied
    );
    assert_eq!(summary.deferred_to_operator_count, 1);

    // Inherited passport is now visible in the store.
    let inherited = store.get(&inherited_pane_key()).expect("inherited present");
    assert_eq!(
        inherited.capabilities[0].verification,
        CapabilityVerification::Verified
    );

    // Step 4: PassportPreflight against the inherited key for its
    // newly-Verified ToolAvailability("bash") — Allowed.
    let resp = server.handle_request(RemoteRequest::PassportPreflight {
        key: inherited_pane_key(),
        required_classes: vec![CapabilityClass::ToolAvailability("bash".into())],
    });
    let RemoteResponse::PreflightOutcome { outcome } = resp else {
        panic!("expected PreflightOutcome, got {resp:?}");
    };
    assert!(
        outcome.is_allowed(),
        "inherited passport should satisfy bash dispatch but got {outcome:?}"
    );
}

/// Even a Verified actor cannot apply a TAMPERED capsule. Server
/// returns capsule_integrity_mismatch error code; the inherited
/// passport never lands in the store; downstream PassportPreflight
/// still returns MissingPassport for the would-have-been-inherited
/// key.
#[test]
fn full_pipeline_tampered_capsule_short_circuits_before_inheritance_ft_1650n_5() {
    let (mut server, store) = build_server();

    server.handle_request(RemoteRequest::RegisterPaneWithPassport {
        identity: actor_identity(),
        state: LifecycleState::Pane(MuxPaneLifecycleState::Running),
        agent_id: "actor-agent".into(),
        declared_capabilities: vec![CapabilityClass::SafetyConstraint(
            "accept_passport_excerpt".into(),
        )],
    });
    probe_promote_actor(&store);

    let mut capsule = build_capsule_with_excerpt(inherited_passport(1));
    // Tamper: swap the PassportExcerpt section for a different
    // generation. Stored integrity hash refers to the original.
    capsule.sections[1] = CapsuleSection::PassportExcerpt {
        passport: inherited_passport(99),
    };
    let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
        actor: actor_key(),
        capsule: Box::new(capsule),
    });
    match resp {
        RemoteResponse::Error { code, .. } => {
            assert_eq!(code, "capsule_integrity_mismatch");
        }
        other => panic!("expected integrity error, got {other:?}"),
    }

    // Inherited passport is NOT in the store.
    assert!(store.get(&inherited_pane_key()).is_none());

    // Downstream PassportPreflight against the would-have-been-
    // inherited key returns MissingPassport.
    let resp = server.handle_request(RemoteRequest::PassportPreflight {
        key: inherited_pane_key(),
        required_classes: vec![CapabilityClass::ToolAvailability("bash".into())],
    });
    let RemoteResponse::PreflightOutcome { outcome } = resp else {
        panic!("expected PreflightOutcome");
    };
    assert_eq!(outcome, PreflightOutcome::MissingPassport);
}

/// An actor whose `accept_passport_excerpt` has NOT been promoted to
/// Verified (still Declared after RegisterPaneWithPassport) cannot
/// receive a PassportExcerpt — section is skipped, store is NOT
/// modified. Confirms the slice-1 fail-closed contract end-to-end:
/// declared !== verified, even for handoff inheritance.
#[test]
fn full_pipeline_declared_only_actor_cannot_inherit_passport_excerpt_ft_1650n_5() {
    let (mut server, store) = build_server();

    server.handle_request(RemoteRequest::RegisterPaneWithPassport {
        identity: actor_identity(),
        state: LifecycleState::Pane(MuxPaneLifecycleState::Running),
        agent_id: "actor-agent".into(),
        declared_capabilities: vec![CapabilityClass::SafetyConstraint(
            "accept_passport_excerpt".into(),
        )],
    });
    // Skip probe promotion intentionally — actor's
    // accept_passport_excerpt stays Declared.

    let capsule = build_capsule_with_excerpt(inherited_passport(1));
    let resp = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
        actor: actor_key(),
        capsule: Box::new(capsule),
    });
    let RemoteResponse::HandoffApplied { summary } = resp else {
        panic!("expected HandoffApplied");
    };
    // ContextSummary (unguarded) accepted; PassportExcerpt skipped.
    assert_eq!(summary.accepted_indices, vec![0]);
    assert_eq!(summary.skipped.len(), 1);
    assert_eq!(summary.skipped[0].section_label, "passport_excerpt");
    assert!(summary.passport_excerpt_applies.is_empty());

    // Store does NOT contain the inherited passport.
    assert!(store.get(&inherited_pane_key()).is_none());
}

/// Re-applying the same capsule after a successful first apply is
/// idempotent: second apply observes AlreadyPresent for the
/// PassportExcerpt section and does NOT clobber any later progress
/// made on the inherited passport between the two applies.
#[test]
fn full_pipeline_repeated_apply_is_idempotent_ft_1650n_5() {
    let (mut server, store) = build_server();

    server.handle_request(RemoteRequest::RegisterPaneWithPassport {
        identity: actor_identity(),
        state: LifecycleState::Pane(MuxPaneLifecycleState::Running),
        agent_id: "actor-agent".into(),
        declared_capabilities: vec![CapabilityClass::SafetyConstraint(
            "accept_passport_excerpt".into(),
        )],
    });
    probe_promote_actor(&store);

    let capsule = build_capsule_with_excerpt(inherited_passport(1));

    // First apply — Applied.
    let resp1 = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
        actor: actor_key(),
        capsule: Box::new(capsule.clone()),
    });
    let RemoteResponse::HandoffApplied { summary: s1 } = resp1 else {
        panic!("expected HandoffApplied (1st)");
    };
    assert_eq!(
        s1.passport_excerpt_applies[0].disposition,
        PassportExcerptDisposition::Applied
    );

    // Second apply — AlreadyPresent.
    let resp2 = server.handle_request(RemoteRequest::ApplyHandoffCapsule {
        actor: actor_key(),
        capsule: Box::new(capsule),
    });
    let RemoteResponse::HandoffApplied { summary: s2 } = resp2 else {
        panic!("expected HandoffApplied (2nd)");
    };
    assert_eq!(
        s2.passport_excerpt_applies[0].disposition,
        PassportExcerptDisposition::AlreadyPresent
    );
}
