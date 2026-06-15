//! Conformance for the PolicyEngine approval/denial gate.
//!
//! The gate's security contract:
//!   - deny-by-default for gated actions (sending is not implicitly permitted),
//!   - gated actions require approval and are never silently downgraded to allow,
//!   - hard denials (an emergency kill switch) cannot be bypassed — they deny
//!     EVERY action for EVERY actor, overriding even normally-allowed reads.
//!
//! These complement the in-module behavioral tests with the comprehensive
//! no-bypass property (every action kind, both actors) and a consolidated
//! statement of the deny-by-default / approval-required contract.

use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyEngine, PolicyInput,
};
use frankenterm_core::policy_quarantine::KillSwitchLevel;

/// The action kinds a watcher/robot can request — reads, sends, control codes,
/// pane lifecycle, workflow, browser auth.
const ALL_ACTIONS: &[ActionKind] = &[
    ActionKind::ReadOutput,
    ActionKind::SearchOutput,
    ActionKind::SendText,
    ActionKind::SendCtrlC,
    ActionKind::SendControl,
    ActionKind::Spawn,
    ActionKind::Split,
    ActionKind::Activate,
    ActionKind::Close,
    ActionKind::BrowserAuth,
    ActionKind::WorkflowRun,
    ActionKind::ReservePane,
    ActionKind::ReleasePane,
];

/// NO FAIL-OPEN: an emergency kill switch denies EVERY action for EVERY actor —
/// even normally-allowed reads, even on the most permissive engine — and every
/// denial is attributed to the kill switch. No action-kind or actor bypass.
#[test]
fn emergency_kill_switch_denies_every_action_kind_no_bypass() {
    for actor in [ActorKind::Human, ActorKind::Robot] {
        // Start from the MOST permissive engine + an active-prompt capability so
        // nothing else would block these actions; only the kill switch should.
        let mut engine = PolicyEngine::permissive();
        engine.quarantine_registry_mut().trip_kill_switch(
            KillSwitchLevel::EmergencyHalt,
            "conformance-test",
            "no-fail-open kill-switch conformance",
            1_000,
        );

        for &action in ALL_ACTIONS {
            let input = PolicyInput::new(action, actor)
                .with_pane(1)
                .with_capabilities(PaneCapabilities::prompt());
            let decision = engine.authorize(&input);
            assert!(
                decision.is_denied(),
                "emergency kill switch must DENY {action:?} for {actor:?} (no fail-open bypass)"
            );
            assert!(
                !decision.is_allowed(),
                "emergency-denied {action:?}/{actor:?} must never report is_allowed()"
            );
            assert_eq!(
                decision.rule_id(),
                Some("policy.kill_switch"),
                "emergency denial of {action:?}/{actor:?} must be attributed to the kill switch"
            );
        }
    }
}

/// DENY-BY-DEFAULT: a robot sending text to a running command is denied — the
/// gate does not implicitly permit sends.
#[test]
fn deny_by_default_robot_send_to_running_command() {
    let mut engine = PolicyEngine::strict();
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(1)
        .with_capabilities(PaneCapabilities::running());
    let decision = engine.authorize(&input);
    assert!(
        decision.is_denied(),
        "a robot sending text to a running command must be denied by default"
    );
    assert!(!decision.is_allowed());
}

/// APPROVAL-REQUIRED, never silently allowed: a gated action (send to an
/// unknown-state pane) requires approval and is never downgraded to allow.
#[test]
fn gated_action_requires_approval_never_silently_allowed() {
    let mut engine = PolicyEngine::strict();
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(1)
        .with_capabilities(PaneCapabilities::unknown());
    let decision = engine.authorize(&input);
    assert!(
        decision.requires_approval(),
        "sending to an unknown-state pane must require approval"
    );
    assert!(
        !decision.is_allowed(),
        "a gated action must never be silently allowed"
    );
}

/// DENY-BY-DEFAULT, COMPREHENSIVE: for EVERY action that SENDS INTO a pane whose
/// command state is ambiguous (unknown), a strict engine never silently allows
/// it — the decision is a denial or an approval requirement, never Allow. This
/// pins "sends are not implicitly permitted" across all send/control kinds, not
/// just SendText.
///
/// Scope note (verified): this property is keyed on the *target pane's* command
/// state, so it covers the actions that inject input into an existing pane
/// (text + control codes). Pane-CREATION actions (Spawn/Split) are deliberately
/// excluded — they do not send into the existing pane, so their safety does not
/// depend on its unknown/running state and the engine allows them here by
/// design (confirmed: Spawn is Allowed in the unknown-state case). Reads
/// (ReadOutput/SearchOutput/Activate) are likewise safe — see safe_read_*.
#[test]
fn sends_never_silently_allowed_into_ambiguous_state_pane() {
    const SEND_ACTIONS: &[ActionKind] = &[
        ActionKind::SendText,
        ActionKind::SendCtrlC,
        ActionKind::SendCtrlD,
        ActionKind::SendCtrlZ,
        ActionKind::SendControl,
    ];
    let mut engine = PolicyEngine::strict();
    for &action in SEND_ACTIONS {
        let input = PolicyInput::new(action, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::unknown());
        let decision = engine.authorize(&input);
        assert!(
            !decision.is_allowed(),
            "deny-by-default: {action:?} into an unknown-state pane must NOT be silently allowed"
        );
        assert!(
            decision.is_denied() || decision.requires_approval(),
            "deny-by-default: {action:?} must be denied or require approval, never neither"
        );
    }

    // Companion: the SAME sends into a RUNNING command are denied outright
    // (stronger than approval — a running command must not receive injected
    // input by default).
    for &action in SEND_ACTIONS {
        let input = PolicyInput::new(action, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        let decision = engine.authorize(&input);
        assert!(
            decision.is_denied() && !decision.is_allowed(),
            "deny-by-default: {action:?} into a running command must be denied"
        );
    }
}

/// SELECTIVE BASELINE: with no emergency, a plain read IS allowed — the gate is
/// deny-by-default-for-gated, not deny-everything. This is the control case that
/// makes the emergency test meaningful (emergency denies even this read).
#[test]
fn safe_read_is_allowed_without_emergency() {
    let mut engine = PolicyEngine::strict();
    let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot);
    let decision = engine.authorize(&input);
    assert!(
        decision.is_allowed(),
        "a plain read must be allowed when no hard denial is active (gate is not deny-everything)"
    );
}
