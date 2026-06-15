//! W10.T (ft-7h5da.11.9) — zero-RCH unit slice for the Blind-Spot Tier-1/2
//! program. Standalone integration test, public API only, no mocks.
//!
//! Covers the LANDED, pure parts of the W10.T acceptance:
//!   * Degraded-Mode Contracts (W10.5, ft-7h5da.11.5 CLOSED) — `contract_for_surface`
//!     returns the admitted + forbidden action sets per dependency-down scenario,
//!     and the sets are disjoint with `admits()`/`forbids()` consistent. The
//!     per-surface defaults encode repo doctrine (RCH down forbids
//!     count-local-cargo-as-proof + restart-rch; Agent Mail down forbids
//!     repair/restart).
//!   * Doctrine-to-Policy compiler bundle shape (W10.1) — `compiled_agents_doctrine_policy()`
//!     compiles AGENTS doctrine into well-formed, stably-identified policy rules,
//!     including the `no-git-worktree` hard rule.
//!
//! GATED remainder (blocked impl / binary / e2e), tracked in the bead comment:
//! doctrine-compiler REJECTION matching per hard rule (W10.1/11.1 — `DoctrineRule::matches`
//! is private; needs a public matcher or CommandGuard doctrine-pack wiring);
//! Virtual Panes parity + wire-protocol adapter (W10.2/11.2); Agent Compatibility
//! Certification (W10.3/11.3); Evidence Lifecycle promote-to-fixture refusal
//! (W10.4/11.4 — `plan_evidence_lifecycle` is landed/public but the bead is
//! blocked; addable next); Zero-Downtime handoff no-gap (W10.6/11.6); A/B SPRT
//! signed verdict (W10.7/11.7); Pre-Approval cross-examination (W10.8/11.8); and
//! the shell e2e (tests/e2e/blindspot_tier1.sh).

use frankenterm_core::command_guard::compiled_agents_doctrine_policy;
use frankenterm_core::degraded_mode::{
    contract_for_surface, DegradedModeAction, DegradedModeState, DegradedModeSurface,
};

// ── Degraded-Mode Contracts (W10.5, closed) ──────────────────────────────────

#[test]
fn degraded_mode_agent_mail_admits_beads_fallback_forbids_repair() {
    let c = contract_for_surface(
        DegradedModeSurface::AgentMail,
        DegradedModeState::Unavailable,
        "agent_mail.unreachable",
    );
    // Beads/git fallback is admitted...
    assert!(c.admits(DegradedModeAction::ClaimReadyBead));
    assert!(c.admits(DegradedModeAction::AddBeadsComment));
    // ...but the AGENTS.md "never repair/restart Agent Mail" doctrine is forbidden.
    assert!(c.forbids(DegradedModeAction::RepairAgentMail));
    assert!(c.forbids(DegradedModeAction::ServiceRestart));
    assert!(
        !c.admits(DegradedModeAction::RepairAgentMail),
        "a forbidden action must never be admitted"
    );
}

#[test]
fn degraded_mode_rch_forbids_local_cargo_as_proof() {
    let c = contract_for_surface(
        DegradedModeSurface::Rch,
        DegradedModeState::Degraded,
        "rch.no_admissible_workers",
    );
    // The repo's hardest proof-integrity doctrine: when RCH is degraded, local
    // cargo is NOT proof, and the RCH service must not be restarted.
    assert!(c.forbids(DegradedModeAction::CountLocalCargoAsProof));
    assert!(c.forbids(DegradedModeAction::RestartRchService));
    assert!(c.forbids(DegradedModeAction::RunRemoteProof));
    // The legitimate degraded-lane work IS admitted.
    assert!(c.admits(DegradedModeAction::RecordDeferredProof));
    assert!(c.admits(DegradedModeAction::LocalHygieneCheck));
    assert!(c.admits(DegradedModeAction::ContinueNonOverlappingCodeWork));
    assert!(!c.admits(DegradedModeAction::CountLocalCargoAsProof));
}

#[test]
fn degraded_mode_admitted_and_forbidden_disjoint_and_consistent_for_all_scenarios() {
    use DegradedModeState::{Blocked, Degraded, NotCollected, Stale, Unavailable};
    use DegradedModeSurface::{AgentMail, Beads, Mcp, Rch, SemanticPane, Storage};

    for surface in [AgentMail, Rch, Mcp, Storage, SemanticPane, Beads] {
        for state in [Degraded, Unavailable, Blocked, Stale, NotCollected] {
            let c = contract_for_surface(surface, state, "scenario.degraded");
            // Admitted and forbidden sets must be disjoint, and the accessors
            // must agree with the underlying sets (fail-closed: forbidden wins).
            for action in &c.admitted_actions {
                assert!(
                    !c.forbidden_actions.contains(action),
                    "{surface:?}/{state:?}: {action:?} is both admitted and forbidden"
                );
                assert!(c.admits(*action), "{surface:?}/{state:?}: admitted action must admit()");
            }
            for action in &c.forbidden_actions {
                assert!(c.forbids(*action), "{surface:?}/{state:?}: forbidden action must forbid()");
                assert!(
                    !c.admits(*action),
                    "{surface:?}/{state:?}: a forbidden action must never admit()"
                );
            }
        }
    }
}

#[test]
fn degraded_mode_contract_preserves_surface_state_and_reason() {
    let c = contract_for_surface(
        DegradedModeSurface::Storage,
        DegradedModeState::Blocked,
        "storage.read_only",
    );
    assert_eq!(c.surface, DegradedModeSurface::Storage);
    assert_eq!(c.state, DegradedModeState::Blocked);
    assert_eq!(c.reason_code, "storage.read_only");
    assert_eq!(c.schema_version, 1);
    // Storage-down doctrine: raw redacted reads admitted, mutations forbidden.
    assert!(c.admits(DegradedModeAction::RawRedactedGetText));
    assert!(c.forbids(DegradedModeAction::StorageMutation));
}

// ── Doctrine-to-Policy compiler bundle shape (W10.1) ─────────────────────────

#[test]
fn doctrine_policy_bundle_compiles_with_stable_hard_rules() {
    let bundle = compiled_agents_doctrine_policy();

    assert!(
        !bundle.rules.is_empty(),
        "AGENTS doctrine must compile to >= 1 policy rule"
    );
    assert!(!bundle.bundle_id.is_empty(), "bundle has a stable id");

    // The no-git-worktree hard rule (a top repo failure class) must be present
    // with its stable id/category so robot/MCP callers get a typed denial.
    assert!(
        bundle.rules.iter().any(|rule| {
            rule.rule_id == "agents.doctrine:no-git-worktree"
                && rule.category == "git_worktree_forbidden"
        }),
        "compiled doctrine must include the no-git-worktree hard rule"
    );

    // Every compiled rule is well-formed (id + category + operator reason).
    for rule in &bundle.rules {
        assert!(!rule.rule_id.is_empty(), "rule_id present");
        assert!(!rule.category.is_empty(), "category present");
        assert!(!rule.reason.is_empty(), "denial reason present");
    }
}
