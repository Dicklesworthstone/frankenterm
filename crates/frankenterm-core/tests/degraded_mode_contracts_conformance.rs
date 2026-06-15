//! W10.T (slice) — Standalone conformance proof for Degraded-Mode Contracts
//! (bead ft-7h5da.11.9, exercising the CLOSED W10.5 sub-bead ft-7h5da.11.5).
//!
//! The W10.T unit spec requires: "Degraded-Mode contracts return admitted +
//! forbidden action sets per dependency-down scenario" and "blocked states
//! return the contract instead of a bare error". This harness proves exactly
//! that against the shipped public API (`frankenterm_core::degraded_mode`), with
//! no source edits and no daemon.
//!
//! Net-new relative to the 5 inline unit tests in `degraded_mode.rs` (which spot
//! check rch/agent_mail/semantic + one snapshot map): this file sweeps EVERY
//! surface, proves the admitted/forbidden sets are DISJOINT (the core safety
//! invariant), pins the `admits()` forbidden-takes-precedence rule, covers the
//! storage/mcp/beads contracts the inline tests skip, exhausts the snapshot
//! state mapping + reason-code precedence, and asserts JSON serde stability.

use frankenterm_core::degraded_mode::{
    contract_for_source_snapshot, contract_for_surface, DegradedModeAction, DegradedModeContract,
    DegradedModeRestoreCondition, DegradedModeState, DegradedModeSurface, DEGRADED_MODE_CONTRACT_ID,
    DEGRADED_MODE_SCHEMA_VERSION,
};
use frankenterm_core::operating_envelope::{
    OperatingEnvelopeSourceKind, OperatingEnvelopeSourceSnapshot,
};

const ALL_SURFACES: [DegradedModeSurface; 6] = [
    DegradedModeSurface::AgentMail,
    DegradedModeSurface::Rch,
    DegradedModeSurface::Mcp,
    DegradedModeSurface::Storage,
    DegradedModeSurface::SemanticPane,
    DegradedModeSurface::Beads,
];

// =============================================================================
// Every surface yields a well-formed, internally-consistent contract
// =============================================================================

#[test]
fn every_surface_contract_is_well_formed() {
    for surface in ALL_SURFACES {
        let contract =
            contract_for_surface(surface, DegradedModeState::Unavailable, "scenario.reason");

        assert_eq!(contract.contract_id, DEGRADED_MODE_CONTRACT_ID);
        assert_eq!(contract.schema_version, DEGRADED_MODE_SCHEMA_VERSION);
        assert_eq!(contract.surface, surface, "surface must round-trip");
        assert_eq!(contract.state, DegradedModeState::Unavailable);
        assert_eq!(contract.reason_code, "scenario.reason");

        // The W10.T acceptance: each degraded surface exposes BOTH an admitted
        // action set and a forbidden action set, plus at least one restore
        // condition (so a blocked agent knows what is still safe AND how to
        // recover).
        assert!(
            !contract.admitted_actions.is_empty(),
            "{} must admit at least one safe fallback action",
            surface.as_str()
        );
        assert!(
            !contract.forbidden_actions.is_empty(),
            "{} must forbid at least one unsafe action",
            surface.as_str()
        );
        assert!(
            !contract.restore_conditions.is_empty(),
            "{} must declare at least one restore condition",
            surface.as_str()
        );
        for condition in &contract.restore_conditions {
            assert!(
                !condition.condition_id.trim().is_empty(),
                "{} restore condition needs a non-empty id",
                surface.as_str()
            );
            assert!(
                !condition.summary.trim().is_empty(),
                "{} restore condition needs a non-empty summary",
                surface.as_str()
            );
        }
    }
}

#[test]
fn admitted_and_forbidden_sets_are_disjoint_for_every_surface() {
    // The central safety invariant: a degraded contract must never both admit
    // and forbid the same action — that would be a self-contradicting receipt an
    // agent could read either way.
    for surface in ALL_SURFACES {
        let contract =
            contract_for_surface(surface, DegradedModeState::Unavailable, "scenario.reason");

        for action in &contract.admitted_actions {
            assert!(
                !contract.forbidden_actions.contains(action),
                "{}: {:?} is in BOTH admitted and forbidden sets",
                surface.as_str(),
                action
            );
        }

        // Consistency between the sets and the query helpers.
        for &action in &contract.admitted_actions {
            assert!(
                contract.admits(action),
                "{}: admitted action {:?} must report admits()==true",
                surface.as_str(),
                action
            );
            assert!(
                !contract.forbids(action),
                "{}: admitted action {:?} must not be forbidden",
                surface.as_str(),
                action
            );
        }
        for &action in &contract.forbidden_actions {
            assert!(
                contract.forbids(action),
                "{}: forbidden action {:?} must report forbids()==true",
                surface.as_str(),
                action
            );
            assert!(
                !contract.admits(action),
                "{}: forbidden action {:?} must never be admitted",
                surface.as_str(),
                action
            );
        }
    }
}

#[test]
fn admits_treats_forbidden_as_overriding_and_unlisted_as_neither() {
    // `admits()` is `admitted AND NOT forbidden`: forbidden takes precedence even
    // if an action is erroneously placed in both lists. Construct that exact
    // contradiction via the public fields and prove the gate fails closed.
    let conflicted = DegradedModeContract {
        contract_id: DEGRADED_MODE_CONTRACT_ID.to_string(),
        schema_version: DEGRADED_MODE_SCHEMA_VERSION,
        surface: DegradedModeSurface::Rch,
        state: DegradedModeState::Unavailable,
        reason_code: "rch.conflict".into(),
        admitted_actions: vec![DegradedModeAction::RunRemoteProof],
        forbidden_actions: vec![DegradedModeAction::RunRemoteProof],
        restore_conditions: vec![DegradedModeRestoreCondition::new(
            "rch.remote_worker_reached",
            "remote worker reachable",
        )],
        notes: Vec::new(),
    };
    assert!(
        !conflicted.admits(DegradedModeAction::RunRemoteProof),
        "forbidden must win over admitted (fail-closed)"
    );
    assert!(conflicted.forbids(DegradedModeAction::RunRemoteProof));

    // An action listed in neither set is neither admitted nor forbidden: the
    // contract is an explicit allow/deny list, not an exhaustive verdict.
    let storage =
        contract_for_surface(DegradedModeSurface::Storage, DegradedModeState::Degraded, "x");
    assert!(!storage.admits(DegradedModeAction::RunRemoteProof));
    assert!(!storage.forbids(DegradedModeAction::RunRemoteProof));
}

// =============================================================================
// Doctrine-aligned per-surface guarantees (incl. storage/mcp/beads the inline
// tests skip)
// =============================================================================

#[test]
fn security_critical_action_sets_match_the_doctrine_per_surface() {
    // RCH down: defer proof, keep local hygiene — but NEVER pass off local cargo
    // as proof, restart the service, drain workers, or claim a remote proof ran.
    let rch = contract_for_surface(DegradedModeSurface::Rch, DegradedModeState::Unavailable, "x");
    for admitted in [
        DegradedModeAction::LocalHygieneCheck,
        DegradedModeAction::RecordDeferredProof,
        DegradedModeAction::ContinueNonOverlappingCodeWork,
    ] {
        assert!(rch.admits(admitted), "rch should admit {admitted:?}");
    }
    for forbidden in [
        DegradedModeAction::CountLocalCargoAsProof,
        DegradedModeAction::RunRemoteProof,
        DegradedModeAction::RestartRchService,
        DegradedModeAction::WorkerDrain,
    ] {
        assert!(rch.forbids(forbidden), "rch must forbid {forbidden:?}");
    }

    // Agent Mail down: Beads-only coordination — never repair/restart the shared
    // service (the AGENTS.md DO-NOT-TOUCH doctrine).
    let mail =
        contract_for_surface(DegradedModeSurface::AgentMail, DegradedModeState::Unavailable, "x");
    assert!(mail.admits(DegradedModeAction::ClaimReadyBead));
    assert!(mail.admits(DegradedModeAction::NonAuthoritativeCoordinationIntent));
    assert!(mail.forbids(DegradedModeAction::RepairAgentMail));
    assert!(mail.forbids(DegradedModeAction::ServiceRestart));

    // MCP off: robot-CLI equivalents + typed unsupported-resource receipts; no
    // service mutation, no semantic claims.
    let mcp = contract_for_surface(DegradedModeSurface::Mcp, DegradedModeState::Unavailable, "x");
    assert!(mcp.admits(DegradedModeAction::RobotCliEquivalent));
    assert!(mcp.admits(DegradedModeAction::EmitUnsupportedResourceReceipt));
    assert!(mcp.forbids(DegradedModeAction::ServiceMutation));
    assert!(mcp.forbids(DegradedModeAction::ServiceRestart));
    assert!(mcp.forbids(DegradedModeAction::SemanticZoneClaim));

    // Storage degraded: read-only redacted tail only; no mutation, no search
    // claims, no derived-store rebuild.
    let storage =
        contract_for_surface(DegradedModeSurface::Storage, DegradedModeState::Degraded, "x");
    assert!(storage.admits(DegradedModeAction::RawRedactedGetText));
    assert!(storage.forbids(DegradedModeAction::StorageMutation));
    assert!(storage.forbids(DegradedModeAction::SearchClaims));
    assert!(storage.forbids(DegradedModeAction::RebuildDerivedStores));

    // Semantic pane unavailable: raw redacted text (confidence-downgraded); no
    // semantic-zone claim, no strict verified-submit.
    let semantic =
        contract_for_surface(DegradedModeSurface::SemanticPane, DegradedModeState::Unavailable, "x");
    assert!(semantic.admits(DegradedModeAction::RawRedactedGetText));
    assert!(semantic.forbids(DegradedModeAction::SemanticZoneClaim));
    assert!(semantic.forbids(DegradedModeAction::StrictVerifiedSubmit));
    assert!(
        semantic
            .notes
            .iter()
            .any(|note| note.contains("confidence-downgraded")),
        "semantic-pane contract must note the confidence downgrade"
    );

    // Beads degraded: comment/file blockers, but never claim ready work against a
    // stale graph or edit overlapping dirty paths.
    let beads =
        contract_for_surface(DegradedModeSurface::Beads, DegradedModeState::Stale, "x");
    assert!(beads.admits(DegradedModeAction::AddBeadsComment));
    assert!(beads.admits(DegradedModeAction::CreateBlockerBead));
    assert!(beads.forbids(DegradedModeAction::ClaimReadyBead));
    assert!(beads.forbids(DegradedModeAction::EditOverlappingDirtyPaths));
}

// =============================================================================
// Operating-envelope snapshot mapping (blocked => contract, healthy => none)
// =============================================================================

#[test]
fn snapshot_maps_each_non_available_state_to_a_contract() {
    // A healthy source yields NO degraded contract (no bare error suppression
    // when nothing is wrong).
    let healthy = OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch);
    assert!(
        contract_for_source_snapshot(&healthy).is_none(),
        "an Available source must not produce a degraded-mode contract"
    );

    // Every degraded/blocked/stale/etc. state maps to a typed contract — the
    // "blocked states return the contract instead of a bare error" acceptance.
    let cases = [
        (
            OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
                .degraded("rch.slow"),
            DegradedModeState::Degraded,
        ),
        (
            OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
                .unavailable("rch.down"),
            DegradedModeState::Unavailable,
        ),
        (
            OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
                .blocked("rch.blocked"),
            DegradedModeState::Blocked,
        ),
        (
            OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
                .stale("rch.stale"),
            DegradedModeState::Stale,
        ),
        (
            OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
                .not_collected("rch.uncollected"),
            DegradedModeState::NotCollected,
        ),
    ];
    for (snapshot, expected_state) in cases {
        let contract = contract_for_source_snapshot(&snapshot)
            .expect("a non-available source must yield a contract");
        assert_eq!(contract.surface, DegradedModeSurface::Rch);
        assert_eq!(contract.state, expected_state);
        // The rch contract's doctrine survives the snapshot path.
        assert!(contract.forbids(DegradedModeAction::CountLocalCargoAsProof));
    }
}

#[test]
fn snapshot_surface_mapping_and_unmapped_kinds() {
    // The four dependency kinds with a degraded-mode doctrine map to a surface.
    let mapped = [
        (OperatingEnvelopeSourceKind::AgentMail, DegradedModeSurface::AgentMail),
        (OperatingEnvelopeSourceKind::Rch, DegradedModeSurface::Rch),
        (OperatingEnvelopeSourceKind::Beads, DegradedModeSurface::Beads),
        (OperatingEnvelopeSourceKind::RobotInventory, DegradedModeSurface::SemanticPane),
    ];
    for (kind, expected_surface) in mapped {
        let snapshot = OperatingEnvelopeSourceSnapshot::new("src", kind).unavailable("down");
        let contract =
            contract_for_source_snapshot(&snapshot).expect("mapped kind must yield a contract");
        assert_eq!(contract.surface, expected_surface);
    }

    // Kinds with no degraded-mode doctrine yield no contract even when down — the
    // mapping is intentionally partial, not a catch-all.
    for kind in [
        OperatingEnvelopeSourceKind::CapacityResource,
        OperatingEnvelopeSourceKind::Git,
        OperatingEnvelopeSourceKind::BlockerRadar,
        OperatingEnvelopeSourceKind::Manual,
        OperatingEnvelopeSourceKind::Fixture,
    ] {
        let snapshot = OperatingEnvelopeSourceSnapshot::new("src", kind).unavailable("down");
        assert!(
            contract_for_source_snapshot(&snapshot).is_none(),
            "unmapped source kind {kind:?} must not produce a degraded contract"
        );
    }
}

#[test]
fn snapshot_reason_code_follows_documented_precedence() {
    // Precedence: unavailable_reason > degraded_reason > reason_codes[0] >
    // "dependency.degraded".
    let unavailable = OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
        .unavailable("rch.unavailable_reason");
    assert_eq!(
        contract_for_source_snapshot(&unavailable).unwrap().reason_code,
        "rch.unavailable_reason"
    );

    let degraded = OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
        .degraded("rch.degraded_reason");
    assert_eq!(
        contract_for_source_snapshot(&degraded).unwrap().reason_code,
        "rch.degraded_reason"
    );

    // `.stale` sets only reason_codes (no unavailable/degraded reason), so the
    // reason falls through to reason_codes[0].
    let stale = OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
        .stale("rch.reason_code_only");
    assert_eq!(
        contract_for_source_snapshot(&stale).unwrap().reason_code,
        "rch.reason_code_only"
    );

    // No reason anywhere => documented fallback.
    let bare = OperatingEnvelopeSourceSnapshot::new("rch", OperatingEnvelopeSourceKind::Rch)
        .state(frankenterm_core::operating_envelope::OperatingEnvelopeSourceState::Degraded);
    assert_eq!(
        contract_for_source_snapshot(&bare).unwrap().reason_code,
        "dependency.degraded"
    );
}

// =============================================================================
// Wire-surface stability
// =============================================================================

#[test]
fn contract_is_deterministic_and_json_round_trips() {
    let a = contract_for_surface(DegradedModeSurface::Rch, DegradedModeState::Unavailable, "rch.x");
    let b = contract_for_surface(DegradedModeSurface::Rch, DegradedModeState::Unavailable, "rch.x");
    assert_eq!(a, b, "contract construction must be deterministic");

    let json_a = serde_json::to_string(&a).expect("serialize contract");
    let json_b = serde_json::to_string(&b).expect("serialize contract");
    assert_eq!(json_a, json_b, "contract JSON must be byte-stable");

    let decoded: DegradedModeContract =
        serde_json::from_str(&json_a).expect("deserialize contract");
    assert_eq!(decoded, a, "contract must round-trip through JSON");
}
