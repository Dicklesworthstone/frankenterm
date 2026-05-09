use frankenterm_core::config::{CommandGateConfig, DcgDenyPolicy, DcgMode};
use frankenterm_core::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyEngine, PolicyInput, is_command_candidate,
};

#[test]
fn repro_policy_bypass_absolute_path() {
    // 1. Direct command "rm" is detected
    assert!(
        is_command_candidate("rm -rf /"),
        "Plain 'rm' should be detected"
    );

    // 2. Absolute path "/bin/rm" - CURRENTLY FAILS
    // The policy engine relies on is_command_candidate returning true to even trigger
    // the regex checks. If this returns false, the regexes are never run.
    assert!(
        is_command_candidate("/bin/rm -rf /"),
        "Absolute path '/bin/rm' should be detected"
    );

    // 3. Relative path "./rm"
    assert!(
        is_command_candidate("./rm -rf /"),
        "Relative path './rm' should be detected"
    );
}

// Recovered from stash@{21}: regression tests for command-gate bypass via
// multi-line input or leading comment line. The PolicyEngine must inspect every
// non-comment line of a SendText payload, not just the first.

#[test]
fn test_multiline_bypass_mitigation() {
    let gate_config = CommandGateConfig {
        enabled: true,
        dcg_mode: DcgMode::Disabled,
        dcg_deny_policy: DcgDenyPolicy::Deny,
    };
    let mut engine = PolicyEngine::permissive().with_command_gate_config(gate_config);

    // Safe first line, dangerous second line.
    let input_text = "echo safe\nrm -rf /";
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(1)
        .with_capabilities(PaneCapabilities::prompt())
        .with_command_text(input_text);

    let decision = engine.authorize(&input);
    assert!(
        decision.is_denied(),
        "multiline command containing 'rm -rf /' should be denied; decision: {decision:?}"
    );
}

#[test]
fn test_comment_bypass_mitigation() {
    let gate_config = CommandGateConfig {
        enabled: true,
        dcg_mode: DcgMode::Disabled,
        dcg_deny_policy: DcgDenyPolicy::Deny,
    };
    let mut engine = PolicyEngine::permissive().with_command_gate_config(gate_config);

    // Comment first line, dangerous second line.
    let input_text = "# harmless comment\nrm -rf /";
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(1)
        .with_capabilities(PaneCapabilities::prompt())
        .with_command_text(input_text);

    let decision = engine.authorize(&input);
    assert!(
        decision.is_denied(),
        "command hidden after a comment line should be denied; decision: {decision:?}"
    );
}
