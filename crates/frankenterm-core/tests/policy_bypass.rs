#[cfg(test)]
mod tests {
    use frankenterm_core::config::{CommandGateConfig, DcgDenyPolicy, DcgMode};
    use frankenterm_core::policy::{
        is_command_candidate, ActionKind, ActorKind, PaneCapabilities, PolicyEngine, PolicyInput,
    };

    #[test]
    fn test_dangerous_interpreters_are_detected() {
        // These dangerous commands using interpreters MUST be detected as command candidates.
        // The COMMAND_TOKENS list includes perl, ruby, php, lua to catch these.
        let dangerous_commands = vec![
            r#"perl -e 'system("rm -rf /")'"#,
            r#"ruby -e 'system("rm -rf /")'"#,
            r#"php -r 'system("rm -rf /");'"#,
            r#"lua -e 'os.execute("rm -rf /")'"#,
        ];

        for cmd in dangerous_commands {
            println!("Testing: {}", cmd);
            assert!(
                is_command_candidate(cmd),
                "Command '{}' should be detected as a command candidate",
                cmd
            );
        }
    }

    #[test]
    fn test_tclsh_is_detected() {
        // tclsh was added to COMMAND_TOKENS to catch Tcl interpreter abuse
        let cmd = "tclsh <<< 'exec rm -rf /'";
        assert!(
            is_command_candidate(cmd),
            "tclsh should be detected as a command candidate"
        );
    }

    #[test]
    fn test_eval_is_detected() {
        // eval was added to COMMAND_TOKENS to catch this pattern
        let cmd = r#"eval "rm -rf /""#;
        assert!(
            is_command_candidate(cmd),
            "eval should be detected as a command candidate"
        );
    }

    #[test]
    fn test_multiline_bypass_mitigation() {
        let mut gate_config = CommandGateConfig::default();
        gate_config.enabled = true;
        gate_config.dcg_mode = DcgMode::Disabled;
        gate_config.dcg_deny_policy = DcgDenyPolicy::Deny;

        // Configure engine with the command gate
        let mut engine = PolicyEngine::permissive().with_command_gate_config(gate_config);

        // Case 1: Safe first line, dangerous second line
        let input_text = "echo safe\nrm -rf /";
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text(input_text);

        let decision = engine.authorize(&input);
        
        // This MUST be Denied (or RequireApproval)
        // With dcg disabled, built-in rules apply. rm -rf / matches command.rm_rf_root (Deny)
        assert!(decision.is_denied(), "Multiline command with 'rm -rf /' should be denied! Decision: {:?}", decision);
    }

    #[test]
    fn test_comment_bypass_mitigation() {
        let mut gate_config = CommandGateConfig::default();
        gate_config.enabled = true;
        gate_config.dcg_mode = DcgMode::Disabled;
        gate_config.dcg_deny_policy = DcgDenyPolicy::Deny;

        let mut engine = PolicyEngine::permissive().with_command_gate_config(gate_config);

        // Case 2: Comment first line, dangerous second line
        let input_text = "# harmless comment\nrm -rf /";
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text(input_text);

        let decision = engine.authorize(&input);

        assert!(decision.is_denied(), "Command hidden after comment should be denied! Decision: {:?}", decision);
    }
}
