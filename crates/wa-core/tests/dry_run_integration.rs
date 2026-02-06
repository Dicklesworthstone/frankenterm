//! Dry-run integration tests (wa-1pe.5)
//!
//! Verifies that dry-run mode:
//! - Never modifies state (DB, locks, panes)
//! - Produces accurate output for both JSON and human formats
//! - Handles error paths gracefully (invalid panes, policy denials, unknown workflows)
//! - Maintains field contracts between dry-run reports and actual execution

use wa_core::dry_run::{
    ActionType, CommandContext, DryRunContext, DryRunReport, PlannedAction, PolicyCheck,
    PolicyEvaluation, TargetResolution,
};

// ============================================================================
// Test Helpers
// ============================================================================

/// Build a realistic dry-run report for testing.
fn build_test_report() -> DryRunReport {
    let mut ctx = DryRunContext::enabled();
    ctx.set_command("wa send --pane 42 \"hello\"".to_string());
    ctx.set_target(
        TargetResolution::new(42, "local")
            .with_title("codex @ /project".to_string())
            .with_cwd("/home/user/project".to_string())
            .with_is_active(true)
            .with_agent_type("codex".to_string()),
    );

    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::passed("rate_limit", "Rate limit: 3/30 (10%)"));
    eval.add_check(PolicyCheck::passed(
        "prompt_active",
        "Prompt is active (agent idle)",
    ));
    eval.add_check(PolicyCheck::passed(
        "alt_screen",
        "Pane is not in alt-screen mode",
    ));
    ctx.set_policy_evaluation(eval);

    ctx.add_action(PlannedAction::new(
        1,
        ActionType::SendText,
        "Send text to pane 42 [policy-gated]",
    ));
    ctx.add_action(PlannedAction::new(
        2,
        ActionType::WaitFor,
        "Wait for prompt boundary",
    ));

    ctx.take_report()
}

/// Build a report with all policy checks failing.
fn build_denied_report() -> DryRunReport {
    let mut ctx = DryRunContext::enabled();
    ctx.set_command("wa send --pane 99 \"test\"".to_string());
    ctx.set_target(TargetResolution::new(99, "unknown"));

    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::failed(
        "rate_limit",
        "Rate limit exceeded: 30/30 (100%)",
    ));
    eval.add_check(PolicyCheck::failed(
        "prompt_active",
        "No prompt detected (agent busy)",
    ));
    eval.add_check(
        PolicyCheck::failed("alt_screen", "Pane is in alt-screen mode")
            .with_details("Exit full-screen application before sending text."),
    );
    ctx.set_policy_evaluation(eval);

    ctx.add_warning("Execution would be denied; resolve policy failures first.");
    ctx.take_report()
}

/// Build a report with redactable secrets.
fn build_report_with_secrets() -> DryRunReport {
    let mut ctx = DryRunContext::enabled();
    ctx.set_command(
        "wa send --pane 1 \"ANTHROPIC_API_KEY=sk-ant-api03-secret-key-here\"".to_string(),
    );
    ctx.set_target(
        TargetResolution::new(1, "local")
            .with_title("agent".to_string())
            .with_cwd("/home/user".to_string()),
    );

    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::passed("rate_limit", "Rate limit: 1/30"));
    ctx.set_policy_evaluation(eval);

    ctx.add_action(
        PlannedAction::new(
            1,
            ActionType::SendText,
            "Send text containing ANTHROPIC_API_KEY=sk-ant-api03-secret-key-here",
        )
        .with_metadata(serde_json::json!({
            "text_preview": "ANTHROPIC_API_KEY=sk-ant-api03-secret-key-here",
            "policy_gated": true,
        })),
    );

    ctx.take_report()
}

// ============================================================================
// Category 1: No State Modification Tests
// ============================================================================

#[test]
fn dry_run_context_produces_report_without_side_effects() {
    // DryRunContext only collects data — confirm it never panics or
    // accesses external resources.
    let mut ctx = DryRunContext::enabled();
    ctx.set_command("wa workflow run handle_compaction --pane 7".to_string());
    ctx.set_target(TargetResolution::new(7, "local"));
    ctx.set_policy_evaluation(PolicyEvaluation::new());
    ctx.add_action(PlannedAction::new(
        1,
        ActionType::AcquireLock,
        "Acquire lock",
    ));
    ctx.add_warning("Test warning");
    let report = ctx.take_report();

    assert_eq!(report.command, "wa workflow run handle_compaction --pane 7");
    assert!(report.target_resolution.is_some());
    assert!(report.policy_evaluation.is_some());
    assert_eq!(report.expected_actions.len(), 1);
    assert_eq!(report.warnings.len(), 1);
}

#[test]
fn dry_run_disabled_context_still_builds_report() {
    let mut ctx = DryRunContext::disabled();
    ctx.set_command("test".to_string());
    ctx.set_target(TargetResolution::new(1, "local"));
    let report = ctx.take_report();

    assert_eq!(report.command, "test");
    assert!(report.target_resolution.is_some());
}

#[test]
fn dry_run_report_is_pure_data_structure() {
    // Verify DryRunReport can be cloned, serialized, and deserialized
    // without any side effects.
    let report = build_test_report();

    // Clone is independent
    let clone = report.clone();
    assert_eq!(clone.command, report.command);
    assert_eq!(clone.expected_actions.len(), report.expected_actions.len());

    // Serialize round-trip via Value (skip_serializing_if omits empty fields)
    let json = serde_json::to_string(&report).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed["command"].as_str().unwrap(), report.command);
    assert_eq!(
        parsed["expected_actions"].as_array().unwrap().len(),
        report.expected_actions.len()
    );
}

#[test]
fn command_context_dry_run_flag_propagates() {
    let ctx = CommandContext::new("wa send --pane 1 \"hello\" --dry-run", true);
    assert!(ctx.is_dry_run());

    let dry_ctx = ctx.dry_run_context();
    assert!(dry_ctx.enabled);

    let non_dry = CommandContext::new("wa send --pane 1 \"hello\"", false);
    assert!(!non_dry.is_dry_run());
}

// ============================================================================
// Category 2: Output Correctness Tests
// ============================================================================

#[test]
fn target_resolution_captures_all_fields() {
    let target = TargetResolution::new(42, "ssh:prod")
        .with_title("editor @ /var/log".to_string())
        .with_cwd("/var/log".to_string())
        .with_agent_type("claude_code".to_string())
        .with_is_active(true);

    assert_eq!(target.pane_id, 42);
    assert_eq!(target.domain, "ssh:prod");
    assert_eq!(target.title.as_deref(), Some("editor @ /var/log"));
    assert_eq!(target.cwd.as_deref(), Some("/var/log"));
    assert_eq!(target.agent_type.as_deref(), Some("claude_code"));
    assert_eq!(target.is_active, Some(true));
}

#[test]
fn target_resolution_minimal_fields() {
    let target = TargetResolution::new(1, "local");

    assert_eq!(target.pane_id, 1);
    assert_eq!(target.domain, "local");
    assert!(target.title.is_none());
    assert!(target.cwd.is_none());
    assert!(target.agent_type.is_none());
    assert!(target.is_active.is_none());
}

#[test]
fn policy_evaluation_all_passed() {
    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::passed("check_a", "Passed A"));
    eval.add_check(PolicyCheck::passed("check_b", "Passed B"));

    assert!(eval.all_passed());
    assert!(eval.failed_checks().is_empty());
}

#[test]
fn policy_evaluation_mixed_results() {
    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::passed("rate_limit", "OK"));
    eval.add_check(PolicyCheck::failed("prompt_active", "No prompt"));
    eval.add_check(PolicyCheck::passed("alt_screen", "OK"));

    assert!(!eval.all_passed());
    assert_eq!(eval.failed_checks().len(), 1);
    assert_eq!(eval.failed_checks()[0].name, "prompt_active");
}

#[test]
fn policy_check_with_details() {
    let check = PolicyCheck::failed("approval", "Requires approval").with_details("Code: ABC123");

    assert!(!check.passed);
    assert_eq!(check.name, "approval");
    assert_eq!(check.details.as_deref(), Some("Code: ABC123"));
}

#[test]
fn planned_action_step_sequence() {
    let report = build_test_report();

    // Steps should be sequential
    for (idx, action) in report.expected_actions.iter().enumerate() {
        assert_eq!(action.step, (idx + 1) as u32);
    }
}

#[test]
fn planned_action_metadata_preserved() {
    let action =
        PlannedAction::new(1, ActionType::SendText, "Send test").with_metadata(serde_json::json!({
            "text_len": 42,
            "policy_gated": true,
        }));

    let meta = action.metadata.unwrap();
    assert_eq!(meta["text_len"], 42);
    assert_eq!(meta["policy_gated"], true);
}

#[test]
fn report_action_count_and_policy_passed() {
    let report = build_test_report();
    assert_eq!(report.action_count(), 2);
    assert!(report.policy_passed());

    let denied = build_denied_report();
    assert!(!denied.policy_passed());
}

// ============================================================================
// Category 3: Error Path Tests
// ============================================================================

#[test]
fn unknown_pane_generates_warning() {
    let mut ctx = DryRunContext::enabled();
    ctx.set_command("wa send --pane 99999 \"test\"".to_string());
    ctx.set_target(TargetResolution::new(99999, "unknown"));
    ctx.add_warning("Pane metadata unavailable; verify pane ID and daemon state.");

    let report = ctx.take_report();
    assert!(report.has_warnings());
    assert!(report.warnings[0].contains("Pane metadata unavailable"));
}

#[test]
fn policy_denial_shows_all_failures() {
    let report = build_denied_report();

    let policy = report.policy_evaluation.as_ref().unwrap();
    let failures = policy.failed_checks();

    assert_eq!(failures.len(), 3);
    assert!(failures.iter().any(|c| c.name == "rate_limit"));
    assert!(failures.iter().any(|c| c.name == "prompt_active"));
    assert!(failures.iter().any(|c| c.name == "alt_screen"));
}

#[test]
fn policy_denial_includes_details() {
    let report = build_denied_report();

    let alt_screen_check = report
        .policy_evaluation
        .as_ref()
        .unwrap()
        .checks
        .iter()
        .find(|c| c.name == "alt_screen")
        .unwrap();

    assert!(alt_screen_check.details.is_some());
    assert!(
        alt_screen_check
            .details
            .as_ref()
            .unwrap()
            .contains("Exit full-screen")
    );
}

#[test]
fn empty_report_handles_gracefully() {
    let report = DryRunReport::new();

    assert!(report.command.is_empty());
    assert!(report.target_resolution.is_none());
    assert!(report.policy_evaluation.is_none());
    assert!(report.expected_actions.is_empty());
    assert!(report.warnings.is_empty());
    assert!(!report.has_warnings());
    assert_eq!(report.action_count(), 0);
    // policy_passed returns true when no evaluation exists
    assert!(report.policy_passed());
}

#[test]
fn report_with_warnings_but_passing_policy() {
    let mut ctx = DryRunContext::enabled();
    ctx.set_command("wa send --pane 1 \"test\"".to_string());

    let mut eval = PolicyEvaluation::new();
    eval.add_check(PolicyCheck::passed("rate_limit", "OK"));
    ctx.set_policy_evaluation(eval);
    ctx.add_warning("Note: Pane state not fully verified in dry-run.");

    let report = ctx.take_report();
    assert!(report.has_warnings());
    assert!(report.policy_passed());
}

// ============================================================================
// Category 4: Format Tests
// ============================================================================

#[test]
fn json_format_produces_valid_json() {
    let report = build_test_report();
    let json_str = wa_core::dry_run::format_json(&report).expect("format_json should succeed");

    let parsed: serde_json::Value = serde_json::from_str(&json_str).expect("should be valid JSON");

    // Required top-level fields
    assert!(parsed.get("command").is_some());
    assert!(parsed.get("target_resolution").is_some());
    assert!(parsed.get("policy_evaluation").is_some());
    assert!(parsed.get("expected_actions").is_some());
}

#[test]
fn json_format_action_fields_complete() {
    let report = build_test_report();
    let json_str = wa_core::dry_run::format_json(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let actions = parsed["expected_actions"].as_array().unwrap();
    for action in actions {
        assert!(action["step"].is_number(), "step should be number");
        assert!(
            action["action_type"].is_string(),
            "action_type should be string"
        );
        assert!(
            action["description"].is_string(),
            "description should be string"
        );
    }
}

#[test]
fn json_format_target_resolution_fields() {
    let report = build_test_report();
    let json_str = wa_core::dry_run::format_json(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let target = &parsed["target_resolution"];
    assert!(target["pane_id"].is_number());
    assert!(target["domain"].is_string());
    assert!(target["title"].is_string());
    assert!(target["cwd"].is_string());
}

#[test]
fn json_format_policy_evaluation_structure() {
    let report = build_test_report();
    let json_str = wa_core::dry_run::format_json(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let policy = &parsed["policy_evaluation"];
    let checks = policy["checks"].as_array().unwrap();
    assert!(!checks.is_empty());

    for check in checks {
        assert!(check["name"].is_string());
        assert!(check["passed"].is_boolean());
        assert!(check["message"].is_string());
    }
}

#[test]
fn human_format_contains_all_sections() {
    let report = build_test_report();
    let output = wa_core::dry_run::format_human(&report);

    // Header
    assert!(output.contains("DRY RUN"));
    // Command
    assert!(output.contains("Command:"));
    assert!(output.contains("wa send"));
    // Target
    assert!(output.contains("Target Resolution:"));
    assert!(output.contains("Pane: 42"));
    assert!(output.contains("Domain: local"));
    assert!(output.contains("CWD: /home/user/project"));
    assert!(output.contains("Agent: codex"));
    // Policy
    assert!(output.contains("Policy Evaluation:"));
    assert!(output.contains("✓"));
    // Actions
    assert!(output.contains("Expected Actions:"));
    assert!(output.contains("[send-text]"));
    assert!(output.contains("[wait-for]"));
    // Footer
    assert!(output.contains("remove --dry-run"));
}

#[test]
fn human_format_denied_shows_failures() {
    let report = build_denied_report();
    let output = wa_core::dry_run::format_human(&report);

    assert!(output.contains("✗"));
    assert!(output.contains("Rate limit exceeded"));
    assert!(output.contains("No prompt detected"));
    assert!(output.contains("alt-screen"));
    assert!(output.contains("⚠")); // Warning symbol
}

#[test]
fn human_format_empty_report_no_crash() {
    let report = DryRunReport::new();
    let output = wa_core::dry_run::format_human(&report);

    // Should still produce header/footer without crashing
    assert!(output.contains("DRY RUN"));
    assert!(output.contains("remove --dry-run"));
}

// ============================================================================
// Category 5: Redaction / Security Tests
// ============================================================================

#[test]
fn json_format_redacts_api_keys() {
    let report = build_report_with_secrets();
    let json_str = wa_core::dry_run::format_json(&report).expect("format");

    // The API key should be redacted
    assert!(
        !json_str.contains("sk-ant-api03-secret-key-here"),
        "API key should be redacted in JSON output"
    );
    assert!(
        json_str.contains("[REDACTED]"),
        "Redacted placeholder should be present"
    );
}

#[test]
fn human_format_redacts_api_keys() {
    let report = build_report_with_secrets();
    let output = wa_core::dry_run::format_human(&report);

    assert!(
        !output.contains("sk-ant-api03-secret-key-here"),
        "API key should be redacted in human output"
    );
    assert!(
        output.contains("[REDACTED]"),
        "Redacted placeholder should be present"
    );
}

#[test]
fn redacted_report_preserves_structure() {
    let report = build_report_with_secrets();
    let redacted = report.redacted();

    // Structure preserved
    assert_eq!(redacted.expected_actions.len(), 1);
    assert!(redacted.target_resolution.is_some());
    assert!(redacted.policy_evaluation.is_some());

    // But secrets are gone
    let action_desc = &redacted.expected_actions[0].description;
    assert!(!action_desc.contains("sk-ant-api03"));
}

// ============================================================================
// Category 6: Action Type Display Tests
// ============================================================================

#[test]
fn action_type_display_all_variants() {
    assert_eq!(format!("{}", ActionType::SendText), "send-text");
    assert_eq!(format!("{}", ActionType::WaitFor), "wait-for");
    assert_eq!(format!("{}", ActionType::AcquireLock), "acquire-lock");
    assert_eq!(format!("{}", ActionType::ReleaseLock), "release-lock");
    assert_eq!(format!("{}", ActionType::StoreData), "store-data");
    assert_eq!(format!("{}", ActionType::WorkflowStep), "workflow-step");
    assert_eq!(
        format!("{}", ActionType::MarkEventHandled),
        "mark-event-handled"
    );
    assert_eq!(
        format!("{}", ActionType::ValidateApproval),
        "validate-approval"
    );
    assert_eq!(format!("{}", ActionType::Other), "other");
}

#[test]
fn action_type_serde_roundtrip() {
    let types = vec![
        ActionType::SendText,
        ActionType::WaitFor,
        ActionType::AcquireLock,
        ActionType::ReleaseLock,
        ActionType::StoreData,
        ActionType::WorkflowStep,
        ActionType::MarkEventHandled,
        ActionType::ValidateApproval,
        ActionType::Other,
    ];

    for action_type in types {
        let json = serde_json::to_string(&action_type).unwrap();
        let deserialized: ActionType = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, action_type);
    }
}

// ============================================================================
// Category 7: Report Builder Contract Tests
// ============================================================================

#[test]
fn report_with_command_builder() {
    let report = DryRunReport::with_command("wa send --pane 1 \"hello\"");
    assert_eq!(report.command, "wa send --pane 1 \"hello\"");
    assert!(report.target_resolution.is_none());
    assert!(report.policy_evaluation.is_none());
    assert!(report.expected_actions.is_empty());
}

#[test]
fn policy_evaluation_from_policy_decision_allow() {
    use wa_core::policy::PolicyDecision;

    let decision = PolicyDecision::allow();
    let check: PolicyCheck = (&decision).into();

    assert!(check.passed);
    assert!(check.message.contains("allowed"));
}

#[test]
fn policy_evaluation_from_policy_decision_deny() {
    use wa_core::policy::PolicyDecision;

    let decision = PolicyDecision::deny("Rate limit exceeded");
    let check: PolicyCheck = (&decision).into();

    assert!(!check.passed);
    assert!(check.message.contains("Rate limit exceeded"));
}

#[test]
fn full_report_json_roundtrip() {
    let report = build_test_report();

    // Use Value for roundtrip since skip_serializing_if omits empty optional fields
    let json = serde_json::to_string_pretty(&report).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(parsed["command"].as_str().unwrap(), report.command);
    assert_eq!(
        parsed["expected_actions"].as_array().unwrap().len(),
        report.expected_actions.len()
    );
    assert_eq!(
        parsed["target_resolution"]["pane_id"].as_u64().unwrap(),
        report.target_resolution.as_ref().unwrap().pane_id
    );
    assert_eq!(
        parsed["policy_evaluation"]["checks"]
            .as_array()
            .unwrap()
            .len(),
        report.policy_evaluation.as_ref().unwrap().checks.len()
    );
}
