//! Conformance: global kill-switch tier enforcement in `PolicyEngine::authorize`
//! (ft-l59nq lock-in; bead ft-mqtge).
//!
//! ft-l59nq fixed a security-critical fail-OPEN: the graduated kill switch only
//! enforced `EmergencyHalt` globally. `SoftStop`/`HardStop` were reachable ONLY
//! via the pane-scoped `is_blocked_for_writes` path, gated on
//! `pane_id.is_some() && action.is_mutating()` — so pane-less actions and
//! non-`is_mutating` actions (`WorkflowRun`, every connector action,
//! `WriteFile`/`DeleteFile`/`ExecCommand`) sailed straight through the exact
//! tiers that name them. The graduated stop collapsed to EmergencyHalt-or-nothing.
//!
//! These tests pin the fixed enforcement contract over the full
//! tier x action x pane matrix:
//!   * each tier halts exactly the right operations,
//!   * fail-closed (a gated action is DENIED via rule `policy.kill_switch`,
//!     never allowed),
//!   * no bypass path (pane-independent; non-`is_mutating` actions are gated;
//!     denial set is monotone in tier severity).

use frankenterm_core::policy::{ActionKind, ActorKind, PolicyDecision, PolicyEngine, PolicyInput};
use frankenterm_core::policy_quarantine::KillSwitchLevel;

/// Every `ActionKind` that requires authorization.
const ALL_ACTIONS: &[ActionKind] = &[
    ActionKind::SendText,
    ActionKind::SendCtrlC,
    ActionKind::SendCtrlD,
    ActionKind::SendCtrlZ,
    ActionKind::SendControl,
    ActionKind::Spawn,
    ActionKind::Split,
    ActionKind::Activate,
    ActionKind::Close,
    ActionKind::BrowserAuth,
    ActionKind::WorkflowRun,
    ActionKind::ReservePane,
    ActionKind::ReleasePane,
    ActionKind::ReadOutput,
    ActionKind::SearchOutput,
    ActionKind::WriteFile,
    ActionKind::DeleteFile,
    ActionKind::ExecCommand,
    ActionKind::ConnectorNotify,
    ActionKind::ConnectorTicket,
    ActionKind::ConnectorTriggerWorkflow,
    ActionKind::ConnectorAuditLog,
    ActionKind::ConnectorInvoke,
    ActionKind::ConnectorCredentialAction,
];

/// In ascending severity order (`KillSwitchLevel` derives `Ord`).
const ALL_LEVELS: &[KillSwitchLevel] = &[
    KillSwitchLevel::Disarmed,
    KillSwitchLevel::SoftStop,
    KillSwitchLevel::HardStop,
    KillSwitchLevel::EmergencyHalt,
];

const KILL_SWITCH_RULE: &str = "policy.kill_switch";

fn engine_at(level: KillSwitchLevel) -> PolicyEngine {
    let mut engine = PolicyEngine::permissive();
    if !matches!(level, KillSwitchLevel::Disarmed) {
        // Indefinite trip (no timeout) — the authorize-time auto-tick will not
        // disarm it. Mirrors the in-crate killswitch tests.
        engine.trip_kill_switch(level, "operator", "incident-drill", 1000);
    }
    engine
}

fn decide(level: KillSwitchLevel, action: ActionKind, pane: Option<u64>) -> PolicyDecision {
    let mut engine = engine_at(level);
    let mut input = PolicyInput::new(action, ActorKind::Robot);
    if let Some(p) = pane {
        input = input.with_pane(p);
    }
    engine.authorize(&input)
}

/// True iff the kill-switch gate (specifically) denied the action.
fn killswitch_denied(level: KillSwitchLevel, action: ActionKind, pane: Option<u64>) -> bool {
    let d = decide(level, action, pane);
    d.is_denied() && d.rule_id() == Some(KILL_SWITCH_RULE)
}

/// The contract the gate must satisfy, restated independently from the documented
/// tier semantics (NOT copied from the gate's `if`s):
///   * Disarmed      — block nothing.
///   * SoftStop      — pause NEW workflow launches; everything else drains.
///   * HardStop      — block every NEW non-read action; reads survive.
///   * EmergencyHalt — block everything, including reads.
fn expected_killswitch_denied(level: KillSwitchLevel, action: ActionKind) -> bool {
    match level {
        KillSwitchLevel::Disarmed => false,
        KillSwitchLevel::SoftStop => action.is_workflow_launch(),
        KillSwitchLevel::HardStop => !action.is_read_only(),
        KillSwitchLevel::EmergencyHalt => true,
    }
}

// ===========================================================================
// Full matrix: each tier halts the right ops, fail-closed, pane-independent.
// ===========================================================================

#[test]
fn killswitch_tier_matrix_is_enforced_fail_closed_and_pane_independent() {
    for &level in ALL_LEVELS {
        for &action in ALL_ACTIONS {
            let want = expected_killswitch_denied(level, action);

            for pane in [None, Some(7u64)] {
                let d = decide(level, action, pane);
                let got = d.is_denied() && d.rule_id() == Some(KILL_SWITCH_RULE);
                assert_eq!(
                    got, want,
                    "level={level:?} action={action:?} pane={pane:?}: kill-switch enforcement \
                     mismatch (denied-by-killswitch={got}, expected={want})"
                );
                if want {
                    // Fail-closed: a kill-switch-gated action must DENY, never allow.
                    assert!(
                        d.is_denied() && !d.is_allowed(),
                        "level={level:?} action={action:?}: gated action must fail closed (deny)"
                    );
                    assert_eq!(
                        d.rule_id(),
                        Some(KILL_SWITCH_RULE),
                        "level={level:?} action={action:?}: deny must be attributed to the \
                         kill-switch rule"
                    );
                }
            }

            // No-bypass: the kill-switch verdict must NOT depend on whether a
            // pane is attached — the core ft-l59nq gap was pane-less actions
            // slipping the pane-scoped path.
            assert_eq!(
                killswitch_denied(level, action, None),
                killswitch_denied(level, action, Some(7)),
                "level={level:?} action={action:?}: kill-switch verdict must be pane-independent"
            );
        }
    }
}

// ===========================================================================
// Regression repros for the exact bypass classes ft-l59nq closed.
// ===========================================================================

#[test]
fn softstop_pauses_exactly_new_workflow_launches() {
    use KillSwitchLevel::SoftStop;

    // Headline repro: a pane-less WorkflowRun used to slip SoftStop entirely.
    assert!(
        killswitch_denied(SoftStop, ActionKind::WorkflowRun, None),
        "SoftStop must pause a pane-less WorkflowRun (the ft-l59nq repro)"
    );
    assert!(killswitch_denied(SoftStop, ActionKind::ConnectorTriggerWorkflow, None));

    // Everything that is NOT a new workflow launch must drain (incl. mutating,
    // connector, exec, file, and read actions).
    for &action in ALL_ACTIONS {
        if action.is_workflow_launch() {
            continue;
        }
        assert!(
            !killswitch_denied(SoftStop, action, None),
            "SoftStop must NOT kill-switch-block non-workflow-launch {action:?}"
        );
    }
}

#[test]
fn hardstop_gates_the_non_mutating_actions_the_old_path_missed() {
    use KillSwitchLevel::HardStop;

    // None of these are `is_mutating()`, so the old pane-scoped
    // `is_blocked_for_writes` gate (pane_id.is_some() && is_mutating) let them
    // bypass HardStop. They must now be denied, pane or no pane.
    for &action in &[
        ActionKind::WorkflowRun,
        ActionKind::ConnectorTriggerWorkflow,
        ActionKind::ConnectorInvoke,
        ActionKind::ConnectorNotify,
        ActionKind::ConnectorCredentialAction,
        ActionKind::WriteFile,
        ActionKind::DeleteFile,
        ActionKind::ExecCommand,
        ActionKind::BrowserAuth,
    ] {
        assert!(
            !action.is_mutating(),
            "{action:?} sanity: not is_mutating (this is the bypass class)"
        );
        assert!(
            killswitch_denied(HardStop, action, None),
            "HardStop must deny non-is_mutating {action:?} (pane-less)"
        );
        assert!(
            killswitch_denied(HardStop, action, Some(3)),
            "HardStop must deny non-is_mutating {action:?} (with pane)"
        );
    }
}

// ===========================================================================
// Reads, EmergencyHalt, Disarmed boundaries.
// ===========================================================================

#[test]
fn reads_survive_soft_and_hard_stop_and_are_blocked_only_at_emergency() {
    for &action in &[
        ActionKind::ReadOutput,
        ActionKind::SearchOutput,
        ActionKind::Activate,
    ] {
        assert!(action.is_read_only(), "{action:?} sanity: is_read_only");
        assert!(
            !killswitch_denied(KillSwitchLevel::SoftStop, action, None),
            "reads must survive SoftStop ({action:?})"
        );
        assert!(
            !killswitch_denied(KillSwitchLevel::HardStop, action, None),
            "reads must survive HardStop so an operator can watch the system drain ({action:?})"
        );
        assert!(
            killswitch_denied(KillSwitchLevel::EmergencyHalt, action, None),
            "EmergencyHalt must block even reads ({action:?})"
        );
    }
}

#[test]
fn emergency_halt_blocks_every_action() {
    for &action in ALL_ACTIONS {
        assert!(
            killswitch_denied(KillSwitchLevel::EmergencyHalt, action, None),
            "EmergencyHalt must block {action:?}"
        );
    }
}

#[test]
fn disarmed_never_kill_switch_blocks() {
    for &action in ALL_ACTIONS {
        for pane in [None, Some(2u64)] {
            assert!(
                !killswitch_denied(KillSwitchLevel::Disarmed, action, pane),
                "Disarmed must never kill-switch-block {action:?} pane={pane:?}"
            );
        }
    }
}

// ===========================================================================
// No-bypass invariant: denial set is monotone in tier severity.
// ===========================================================================

#[test]
fn killswitch_denial_set_is_monotone_in_tier_severity() {
    // A stricter tier must be a superset of a looser tier's denials — there is
    // no tier at which a previously-blocked action silently becomes allowed.
    for &action in ALL_ACTIONS {
        let mut denied_at_looser = false;
        for &level in ALL_LEVELS {
            let denied = killswitch_denied(level, action, None);
            assert!(
                denied || !denied_at_looser,
                "monotonicity violated for {action:?}: blocked at a looser tier but allowed at \
                 {level:?}"
            );
            denied_at_looser = denied;
        }
    }
}

// ===========================================================================
// The classifiers the gate keys on must match the documented tier sets.
// ===========================================================================

#[test]
fn action_classifiers_match_documented_tier_sets() {
    for &action in ALL_ACTIONS {
        let is_launch = matches!(
            action,
            ActionKind::WorkflowRun | ActionKind::ConnectorTriggerWorkflow
        );
        assert_eq!(
            action.is_workflow_launch(),
            is_launch,
            "{action:?}: is_workflow_launch must be exactly the SoftStop-paused set"
        );

        let is_read = matches!(
            action,
            ActionKind::ReadOutput | ActionKind::SearchOutput | ActionKind::Activate
        );
        assert_eq!(
            action.is_read_only(),
            is_read,
            "{action:?}: is_read_only must be exactly the HardStop-permitted set"
        );
    }
}
