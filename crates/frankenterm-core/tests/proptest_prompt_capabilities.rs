//! ft-xxfwy.13 — the send-policy decision table for prompt, alt-screen and
//! capture-gap evidence.
//!
//! The README tour and the `policy.prompt_unknown` hint promise a specific
//! outcome for each combination of pane evidence and caller. This property
//! test pins that table: for every generated (shell state, alt-screen, recent
//! gap, actor, engine toggles) tuple, `PolicyEngine::authorize` must return the
//! decision class and determining rule the oracle below predicts.

use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyEngine, PolicyInput,
};
use proptest::prelude::*;

/// OSC 133 shell state as reconstructed from captured output.
#[derive(Debug, Clone, Copy)]
enum ShellEvidence {
    /// No OSC 133 markers seen for the pane.
    None,
    /// Prompt marker is the latest state.
    AtPrompt,
    /// A command started and has not finished.
    CommandRunning,
    /// Command finished; no new prompt marker yet.
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Allow,
    Deny,
    RequireApproval,
}

#[derive(Debug, Clone)]
struct Case {
    evidence: ShellEvidence,
    alt_screen: Option<bool>,
    recent_gap: bool,
    actor: ActorKind,
    action: ActionKind,
    require_prompt_active: bool,
    block_alt_screen: bool,
    block_recent_gap: bool,
}

fn capabilities(case: &Case) -> PaneCapabilities {
    let (prompt_active, command_running) = match case.evidence {
        ShellEvidence::AtPrompt => (true, false),
        ShellEvidence::CommandRunning => (false, true),
        ShellEvidence::None | ShellEvidence::Output => (false, false),
    };
    PaneCapabilities {
        prompt_active,
        command_running,
        alt_screen: case.alt_screen,
        has_recent_gap: case.recent_gap,
        is_reserved: Some(false),
        ..Default::default()
    }
}

/// The documented decision table, evaluated in the engine's rule order.
fn expected(case: &Case) -> (Class, Option<&'static str>) {
    let caps = capabilities(case);
    let untrusted = case.actor != ActorKind::Human;
    if caps.alt_screen == Some(true) {
        return if case.block_alt_screen && untrusted {
            (Class::RequireApproval, Some("policy.block_alt_screen"))
        } else {
            (Class::Deny, Some("policy.alt_screen"))
        };
    }
    if caps.alt_screen.is_none() && untrusted {
        return (Class::RequireApproval, Some("policy.alt_screen_unknown"));
    }
    if case.block_recent_gap && caps.has_recent_gap && untrusted {
        return (Class::RequireApproval, Some("policy.recent_gap"));
    }
    if case.require_prompt_active && !caps.prompt_active {
        if caps.command_running {
            return (Class::Deny, Some("policy.prompt_required"));
        }
        if untrusted {
            return (Class::RequireApproval, Some("policy.prompt_unknown"));
        }
    }
    (Class::Allow, None)
}

fn arb_case() -> impl Strategy<Value = Case> {
    (
        prop_oneof![
            Just(ShellEvidence::None),
            Just(ShellEvidence::AtPrompt),
            Just(ShellEvidence::CommandRunning),
            Just(ShellEvidence::Output),
        ],
        prop_oneof![Just(None), Just(Some(true)), Just(Some(false))],
        any::<bool>(),
        prop_oneof![
            Just(ActorKind::Human),
            Just(ActorKind::Robot),
            Just(ActorKind::Mcp),
            Just(ActorKind::Workflow),
        ],
        prop_oneof![Just(ActionKind::SendText), Just(ActionKind::SendControl)],
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(
                evidence,
                alt_screen,
                recent_gap,
                actor,
                action,
                require_prompt_active,
                block_alt_screen,
                block_recent_gap,
            )| Case {
                evidence,
                alt_screen,
                recent_gap,
                actor,
                action,
                require_prompt_active,
                block_alt_screen,
                block_recent_gap,
            },
        )
}

fn decide(case: &Case) -> (Class, Option<String>) {
    let mut engine = PolicyEngine::new(1000, 5000, case.require_prompt_active)
        .with_block_alt_screen(case.block_alt_screen)
        .with_block_recent_gap(case.block_recent_gap);
    let input = PolicyInput::new(case.action, case.actor)
        .with_pane(1)
        .with_capabilities(capabilities(case));
    let decision = engine.authorize(&input);
    let class = if decision.is_allowed() {
        Class::Allow
    } else if decision.requires_approval() {
        Class::RequireApproval
    } else {
        Class::Deny
    };
    (class, decision.rule_id().map(str::to_string))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn send_policy_matches_the_prompt_evidence_table(case in arb_case()) {
        let (want_class, want_rule) = expected(&case);
        let (got_class, got_rule) = decide(&case);
        prop_assert_eq!(got_class, want_class, "case {:?}: rule {:?}", case, got_rule);
        if let Some(rule) = want_rule {
            prop_assert_eq!(got_rule.as_deref(), Some(rule), "case {:?}", case);
        }
    }
}

/// The whole table is small enough to check exhaustively as well, so a
/// regression names the exact row even if proptest never sampled it.
#[test]
fn send_policy_prompt_evidence_table_is_exhaustively_consistent() {
    let evidences = [
        ShellEvidence::None,
        ShellEvidence::AtPrompt,
        ShellEvidence::CommandRunning,
        ShellEvidence::Output,
    ];
    let actors = [
        ActorKind::Human,
        ActorKind::Robot,
        ActorKind::Mcp,
        ActorKind::Workflow,
    ];
    let mut rows = 0;
    for evidence in evidences {
        for alt_screen in [None, Some(true), Some(false)] {
            for recent_gap in [false, true] {
                for actor in actors {
                    for action in [ActionKind::SendText, ActionKind::SendControl] {
                        for require_prompt_active in [false, true] {
                            for block_alt_screen in [false, true] {
                                for block_recent_gap in [false, true] {
                                    let case = Case {
                                        evidence,
                                        alt_screen,
                                        recent_gap,
                                        actor,
                                        action,
                                        require_prompt_active,
                                        block_alt_screen,
                                        block_recent_gap,
                                    };
                                    let (want_class, want_rule) = expected(&case);
                                    let (got_class, got_rule) = decide(&case);
                                    assert_eq!(
                                        got_class, want_class,
                                        "case {case:?}: rule {got_rule:?}"
                                    );
                                    if let Some(rule) = want_rule {
                                        assert_eq!(
                                            got_rule.as_deref(),
                                            Some(rule),
                                            "case {case:?}"
                                        );
                                    }
                                    rows += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(rows, 4 * 3 * 2 * 4 * 2 * 2 * 2 * 2);
}

#[test]
fn integrated_pane_is_allowed_and_unintegrated_pane_names_the_fix() {
    let base = Case {
        evidence: ShellEvidence::AtPrompt,
        alt_screen: Some(false),
        recent_gap: false,
        actor: ActorKind::Robot,
        action: ActionKind::SendText,
        require_prompt_active: true,
        block_alt_screen: false,
        block_recent_gap: true,
    };
    assert_eq!(decide(&base).0, Class::Allow);

    let unintegrated = Case {
        evidence: ShellEvidence::None,
        ..base
    };
    let mut engine = PolicyEngine::new(1000, 5000, true);
    let decision = engine.authorize(
        &PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(capabilities(&unintegrated)),
    );
    assert!(decision.requires_approval());
    assert_eq!(decision.rule_id(), Some("policy.prompt_unknown"));
    assert!(
        decision
            .reason()
            .unwrap_or_default()
            .contains("ft setup shell"),
        "the prompt_unknown reason must name the fix: {:?}",
        decision.reason()
    );
}
