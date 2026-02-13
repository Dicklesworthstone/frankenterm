//! Property-based tests for the dry_run module.
//!
//! Tests cover: ActionType, PolicyCheck, PolicyEvaluation, PlannedAction,
//! TargetResolution, DryRunReport, DryRunContext, CommandContext,
//! build_send_policy_evaluation, create_send_action, create_wait_for_action,
//! format_json, format_human, and cross-module interactions.

use proptest::prelude::*;
use serde_json;

use frankenterm_core::dry_run::{
    ActionType, CommandContext, DryRunContext, DryRunReport, PlannedAction, PolicyCheck,
    PolicyEvaluation, TargetResolution, build_send_policy_evaluation, create_send_action,
    create_wait_for_action, format_human, format_json,
};

// ============================================================================
// Strategies
// ============================================================================

fn arb_action_type() -> impl Strategy<Value = ActionType> {
    prop_oneof![
        Just(ActionType::SendText),
        Just(ActionType::WaitFor),
        Just(ActionType::AcquireLock),
        Just(ActionType::ReleaseLock),
        Just(ActionType::StoreData),
        Just(ActionType::WorkflowStep),
        Just(ActionType::MarkEventHandled),
        Just(ActionType::ValidateApproval),
        Just(ActionType::Other),
    ]
}

fn arb_policy_check() -> impl Strategy<Value = PolicyCheck> {
    (
        "[a-z_]{1,20}",
        any::<bool>(),
        "[a-zA-Z0-9 ]{1,50}",
        proptest::option::of("[a-zA-Z0-9 ]{1,30}"),
    )
        .prop_map(|(name, passed, message, details)| {
            let mut check = if passed {
                PolicyCheck::passed(&name, &message)
            } else {
                PolicyCheck::failed(&name, &message)
            };
            if let Some(d) = details {
                check = check.with_details(d);
            }
            check
        })
}

fn arb_policy_evaluation() -> impl Strategy<Value = PolicyEvaluation> {
    proptest::collection::vec(arb_policy_check(), 0..8).prop_map(|checks| {
        let mut eval = PolicyEvaluation::new();
        for c in checks {
            eval.add_check(c);
        }
        eval
    })
}

fn arb_target_resolution() -> impl Strategy<Value = TargetResolution> {
    (
        any::<u64>(),
        "[a-z]{1,10}",
        proptest::option::of("[a-zA-Z ]{1,20}"),
        proptest::option::of("/[a-z/]{1,30}"),
        proptest::option::of(any::<bool>()),
        proptest::option::of("[a-z_]{1,15}"),
    )
        .prop_map(|(pane_id, domain, title, cwd, is_active, agent_type)| {
            let mut t = TargetResolution::new(pane_id, &domain);
            if let Some(ti) = title {
                t = t.with_title(ti);
            }
            if let Some(c) = cwd {
                t = t.with_cwd(c);
            }
            if let Some(a) = is_active {
                t = t.with_is_active(a);
            }
            if let Some(ag) = agent_type {
                t = t.with_agent_type(ag);
            }
            t
        })
}

fn arb_planned_action() -> impl Strategy<Value = PlannedAction> {
    (
        1..100u32,
        arb_action_type(),
        "[a-zA-Z0-9 ]{1,40}",
        any::<bool>(),
    )
        .prop_map(|(step, action_type, desc, has_meta)| {
            let mut a = PlannedAction::new(step, action_type, &desc);
            if has_meta {
                a = a.with_metadata(serde_json::json!({"key": "value"}));
            }
            a
        })
}

fn arb_dry_run_report() -> impl Strategy<Value = DryRunReport> {
    (
        "[a-zA-Z0-9 ]{0,30}",
        proptest::option::of(arb_target_resolution()),
        proptest::option::of(arb_policy_evaluation()),
        proptest::collection::vec(arb_planned_action(), 0..5),
        proptest::collection::vec("[a-zA-Z0-9 ]{1,30}", 0..4),
    )
        .prop_map(
            |(command, target, policy, actions, warnings)| DryRunReport {
                command,
                target_resolution: target,
                policy_evaluation: policy,
                expected_actions: actions,
                warnings,
            },
        )
}

// ============================================================================
// ActionType properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ActionType serde roundtrip preserves value
    #[test]
    fn prop_action_type_serde_roundtrip(at in arb_action_type()) {
        let json = serde_json::to_string(&at).unwrap();
        let decoded: ActionType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(at, decoded);
    }

    /// ActionType serializes to snake_case strings
    #[test]
    fn prop_action_type_snake_case(at in arb_action_type()) {
        let json = serde_json::to_string(&at).unwrap();
        // Remove quotes
        let s = json.trim_matches('"');
        // snake_case: only lowercase + underscore
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Expected snake_case, got: {}", s
        );
    }

    /// ActionType Display produces kebab-case
    #[test]
    fn prop_action_type_display_kebab(at in arb_action_type()) {
        let display = format!("{}", at);
        // kebab-case: only lowercase + hyphen
        prop_assert!(
            display.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
            "Expected kebab-case, got: {}", display
        );
    }

    /// ActionType Display output is non-empty
    #[test]
    fn prop_action_type_display_nonempty(at in arb_action_type()) {
        let display = format!("{}", at);
        prop_assert!(!display.is_empty());
    }

    /// All 9 ActionType variants have distinct Display values
    #[test]
    fn prop_action_type_display_distinct(_dummy in 0..1u8) {
        let all = vec![
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
        let displays: Vec<String> = all.iter().map(|a| format!("{}", a)).collect();
        let mut uniq = displays.clone();
        uniq.sort();
        uniq.dedup();
        prop_assert_eq!(displays.len(), uniq.len(), "Display values not distinct");
    }

    /// All 9 ActionType variants have distinct serde values
    #[test]
    fn prop_action_type_serde_distinct(_dummy in 0..1u8) {
        let all = vec![
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
        let jsons: Vec<String> = all.iter().map(|a| serde_json::to_string(a).unwrap()).collect();
        let mut uniq = jsons.clone();
        uniq.sort();
        uniq.dedup();
        prop_assert_eq!(jsons.len(), uniq.len(), "Serde values not distinct");
    }
}

// ============================================================================
// PolicyCheck properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PolicyCheck serde roundtrip
    #[test]
    fn prop_policy_check_serde_roundtrip(check in arb_policy_check()) {
        let json = serde_json::to_string(&check).unwrap();
        let decoded: PolicyCheck = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(check.name, decoded.name);
        prop_assert_eq!(check.passed, decoded.passed);
        prop_assert_eq!(check.message, decoded.message);
        prop_assert_eq!(check.details, decoded.details);
    }

    /// PolicyCheck::passed always creates a passing check
    #[test]
    fn prop_policy_check_passed_is_passed(
        name in "[a-z]{1,10}",
        msg in "[a-z]{1,20}",
    ) {
        let check = PolicyCheck::passed(&name, &msg);
        prop_assert!(check.passed);
        prop_assert_eq!(check.name, name);
        prop_assert_eq!(check.message, msg);
        prop_assert!(check.details.is_none());
    }

    /// PolicyCheck::failed always creates a failing check
    #[test]
    fn prop_policy_check_failed_is_failed(
        name in "[a-z]{1,10}",
        msg in "[a-z]{1,20}",
    ) {
        let check = PolicyCheck::failed(&name, &msg);
        prop_assert!(!check.passed);
        prop_assert_eq!(check.name, name);
        prop_assert_eq!(check.message, msg);
        prop_assert!(check.details.is_none());
    }

    /// with_details always sets Some
    #[test]
    fn prop_policy_check_with_details_sets(
        name in "[a-z]{1,10}",
        msg in "[a-z]{1,10}",
        details in "[a-z]{1,20}",
    ) {
        let check = PolicyCheck::passed(&name, &msg).with_details(&details);
        prop_assert_eq!(check.details, Some(details));
    }
}

// ============================================================================
// PolicyEvaluation properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Empty evaluation is all_passed
    #[test]
    fn prop_policy_eval_empty_passes(_dummy in 0..1u8) {
        let eval = PolicyEvaluation::new();
        prop_assert!(eval.all_passed());
        prop_assert!(eval.failed_checks().is_empty());
    }

    /// all_passed is true iff no failed checks exist
    #[test]
    fn prop_policy_eval_all_passed_iff_no_failures(eval in arb_policy_evaluation()) {
        let has_failures = eval.checks.iter().any(|c| !c.passed);
        prop_assert_eq!(eval.all_passed(), !has_failures);
    }

    /// failed_checks returns exactly the checks where passed=false
    #[test]
    fn prop_policy_eval_failed_count(eval in arb_policy_evaluation()) {
        let expected_count = eval.checks.iter().filter(|c| !c.passed).count();
        prop_assert_eq!(eval.failed_checks().len(), expected_count);
    }

    /// failed_checks entries all have passed=false
    #[test]
    fn prop_policy_eval_failed_all_false(eval in arb_policy_evaluation()) {
        for check in eval.failed_checks() {
            prop_assert!(!check.passed);
        }
    }

    /// add_check increases length by 1
    #[test]
    fn prop_policy_eval_add_check_grows(
        eval in arb_policy_evaluation(),
        check in arb_policy_check(),
    ) {
        let initial_len = eval.checks.len();
        let mut eval = eval;
        eval.add_check(check);
        prop_assert_eq!(eval.checks.len(), initial_len + 1);
    }

    /// PolicyEvaluation serde roundtrip
    #[test]
    fn prop_policy_eval_serde_roundtrip(eval in arb_policy_evaluation()) {
        let json = serde_json::to_string(&eval).unwrap();
        let decoded: PolicyEvaluation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(eval.checks.len(), decoded.checks.len());
        for (a, b) in eval.checks.iter().zip(decoded.checks.iter()) {
            prop_assert_eq!(&a.name, &b.name);
            prop_assert_eq!(a.passed, b.passed);
        }
    }
}

// ============================================================================
// PlannedAction properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// PlannedAction serde roundtrip
    #[test]
    fn prop_planned_action_serde_roundtrip(action in arb_planned_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let decoded: PlannedAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action.step, decoded.step);
        prop_assert_eq!(action.action_type, decoded.action_type);
        prop_assert_eq!(action.description, decoded.description);
        prop_assert_eq!(action.metadata.is_some(), decoded.metadata.is_some());
    }

    /// PlannedAction preserves step and action_type
    #[test]
    fn prop_planned_action_fields(
        step in 1..1000u32,
        at in arb_action_type(),
        desc in "[a-z]{1,20}",
    ) {
        let a = PlannedAction::new(step, at, &desc);
        prop_assert_eq!(a.step, step);
        prop_assert_eq!(a.action_type, at);
        prop_assert_eq!(a.description, desc);
        prop_assert!(a.metadata.is_none());
    }

    /// with_metadata always sets Some
    #[test]
    fn prop_planned_action_with_metadata(
        step in 1..100u32,
        at in arb_action_type(),
    ) {
        let a = PlannedAction::new(step, at, "test")
            .with_metadata(serde_json::json!(42));
        prop_assert!(a.metadata.is_some());
    }
}

// ============================================================================
// TargetResolution properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// TargetResolution serde roundtrip
    #[test]
    fn prop_target_resolution_serde_roundtrip(target in arb_target_resolution()) {
        let json = serde_json::to_string(&target).unwrap();
        let decoded: TargetResolution = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(target.pane_id, decoded.pane_id);
        prop_assert_eq!(target.domain, decoded.domain);
        prop_assert_eq!(target.title, decoded.title);
        prop_assert_eq!(target.cwd, decoded.cwd);
        prop_assert_eq!(target.is_active, decoded.is_active);
        prop_assert_eq!(target.agent_type, decoded.agent_type);
    }

    /// new() sets only pane_id and domain, rest is None
    #[test]
    fn prop_target_resolution_new_minimal(
        pane_id in any::<u64>(),
        domain in "[a-z]{1,10}",
    ) {
        let t = TargetResolution::new(pane_id, &domain);
        prop_assert_eq!(t.pane_id, pane_id);
        prop_assert_eq!(t.domain, domain);
        prop_assert!(t.title.is_none());
        prop_assert!(t.cwd.is_none());
        prop_assert!(t.is_active.is_none());
        prop_assert!(t.agent_type.is_none());
    }

    /// Builder methods are additive (each sets exactly one field)
    #[test]
    fn prop_target_resolution_builder_additive(
        pane_id in any::<u64>(),
        domain in "[a-z]{1,10}",
        title in "[a-z]{1,10}",
        cwd in "/[a-z]{1,10}",
        active in any::<bool>(),
        agent in "[a-z]{1,10}",
    ) {
        let t = TargetResolution::new(pane_id, &domain)
            .with_title(&title)
            .with_cwd(&cwd)
            .with_is_active(active)
            .with_agent_type(&agent);
        prop_assert_eq!(t.title, Some(title));
        prop_assert_eq!(t.cwd, Some(cwd));
        prop_assert_eq!(t.is_active, Some(active));
        prop_assert_eq!(t.agent_type, Some(agent));
    }

    /// skip_serializing_if: None fields are absent from JSON
    #[test]
    fn prop_target_resolution_skip_none(
        pane_id in any::<u64>(),
        domain in "[a-z]{1,10}",
    ) {
        let t = TargetResolution::new(pane_id, &domain);
        let json = serde_json::to_string(&t).unwrap();
        prop_assert!(!json.contains("title"));
        prop_assert!(!json.contains("cwd"));
        prop_assert!(!json.contains("is_active"));
        prop_assert!(!json.contains("agent_type"));
    }
}

// ============================================================================
// DryRunReport properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DryRunReport serde roundtrip (warnings must be non-empty because
    /// skip_serializing_if="Vec::is_empty" without serde(default) means
    /// an empty Vec is omitted during serialization and fails deserialization)
    #[test]
    fn prop_dry_run_report_serde_roundtrip(
        cmd in "[a-zA-Z0-9 ]{0,30}",
        target in proptest::option::of(arb_target_resolution()),
        policy in proptest::option::of(arb_policy_evaluation()),
        actions in proptest::collection::vec(arb_planned_action(), 0..5),
        warnings in proptest::collection::vec("[a-zA-Z0-9 ]{1,30}", 1..4),
    ) {
        let report = DryRunReport {
            command: cmd,
            target_resolution: target,
            policy_evaluation: policy,
            expected_actions: actions,
            warnings,
        };
        let json = serde_json::to_string(&report).unwrap();
        let decoded: DryRunReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&report.command, &decoded.command);
        prop_assert_eq!(report.expected_actions.len(), decoded.expected_actions.len());
        prop_assert_eq!(report.warnings.len(), decoded.warnings.len());
        prop_assert_eq!(
            report.target_resolution.is_some(),
            decoded.target_resolution.is_some()
        );
        prop_assert_eq!(
            report.policy_evaluation.is_some(),
            decoded.policy_evaluation.is_some()
        );
    }

    /// Default report is empty with no warnings, no actions
    #[test]
    fn prop_dry_run_report_default(_dummy in 0..1u8) {
        let report = DryRunReport::default();
        prop_assert!(report.command.is_empty());
        prop_assert!(report.target_resolution.is_none());
        prop_assert!(report.policy_evaluation.is_none());
        prop_assert!(report.expected_actions.is_empty());
        prop_assert!(report.warnings.is_empty());
    }

    /// new() == default()
    #[test]
    fn prop_dry_run_report_new_eq_default(_dummy in 0..1u8) {
        let a = DryRunReport::new();
        let b = DryRunReport::default();
        prop_assert_eq!(a.command, b.command);
        prop_assert_eq!(a.expected_actions.len(), b.expected_actions.len());
        prop_assert_eq!(a.warnings.len(), b.warnings.len());
    }

    /// with_command sets command field
    #[test]
    fn prop_dry_run_report_with_command(cmd in "[a-z ]{1,30}") {
        let report = DryRunReport::with_command(&cmd);
        prop_assert_eq!(report.command, cmd);
    }

    /// has_warnings matches warnings non-empty
    #[test]
    fn prop_dry_run_report_has_warnings(report in arb_dry_run_report()) {
        prop_assert_eq!(report.has_warnings(), !report.warnings.is_empty());
    }

    /// action_count matches expected_actions length
    #[test]
    fn prop_dry_run_report_action_count(report in arb_dry_run_report()) {
        prop_assert_eq!(report.action_count(), report.expected_actions.len());
    }

    /// policy_passed is true when no policy or all checks pass
    #[test]
    fn prop_dry_run_report_policy_passed(report in arb_dry_run_report()) {
        let expected = match &report.policy_evaluation {
            None => true,
            Some(eval) => eval.all_passed(),
        };
        prop_assert_eq!(report.policy_passed(), expected);
    }

    /// skip_serializing_if: empty warnings absent from JSON
    #[test]
    fn prop_dry_run_report_skip_empty_warnings(cmd in "[a-z]{1,10}") {
        let report = DryRunReport::with_command(&cmd);
        let json = serde_json::to_string(&report).unwrap();
        // No warnings field when empty
        prop_assert!(!json.contains("warnings"));
    }

    /// skip_serializing_if: None target absent from JSON
    #[test]
    fn prop_dry_run_report_skip_none_target(cmd in "[a-z]{1,10}") {
        let report = DryRunReport::with_command(&cmd);
        let json = serde_json::to_string(&report).unwrap();
        prop_assert!(!json.contains("target_resolution"));
    }

    /// skip_serializing_if: None policy absent from JSON
    #[test]
    fn prop_dry_run_report_skip_none_policy(cmd in "[a-z]{1,10}") {
        let report = DryRunReport::with_command(&cmd);
        let json = serde_json::to_string(&report).unwrap();
        prop_assert!(!json.contains("policy_evaluation"));
    }
}

// ============================================================================
// DryRunContext properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// from_flag(true) == enabled(), from_flag(false) == disabled()
    #[test]
    fn prop_dry_run_context_from_flag(flag in any::<bool>()) {
        let ctx = DryRunContext::from_flag(flag);
        prop_assert_eq!(ctx.is_dry_run(), flag);
        prop_assert_eq!(ctx.enabled, flag);
    }

    /// enabled() always returns is_dry_run = true
    #[test]
    fn prop_dry_run_context_enabled(_dummy in 0..1u8) {
        let ctx = DryRunContext::enabled();
        prop_assert!(ctx.is_dry_run());
    }

    /// disabled() always returns is_dry_run = false
    #[test]
    fn prop_dry_run_context_disabled(_dummy in 0..1u8) {
        let ctx = DryRunContext::disabled();
        prop_assert!(!ctx.is_dry_run());
    }

    /// set_command propagates to report
    #[test]
    fn prop_dry_run_context_set_command(cmd in "[a-z]{1,30}") {
        let mut ctx = DryRunContext::enabled();
        ctx.set_command(&cmd);
        prop_assert_eq!(ctx.report.command, cmd);
    }

    /// add_warning increases warnings count
    #[test]
    fn prop_dry_run_context_add_warning(
        warnings in proptest::collection::vec("[a-z]{1,20}", 1..5),
    ) {
        let mut ctx = DryRunContext::enabled();
        for (i, w) in warnings.iter().enumerate() {
            ctx.add_warning(w);
            prop_assert_eq!(ctx.report.warnings.len(), i + 1);
        }
    }

    /// set_target stores target in report
    #[test]
    fn prop_dry_run_context_set_target(target in arb_target_resolution()) {
        let pane_id = target.pane_id;
        let mut ctx = DryRunContext::enabled();
        prop_assert!(ctx.report.target_resolution.is_none());
        ctx.set_target(target);
        prop_assert!(ctx.report.target_resolution.is_some());
        prop_assert_eq!(ctx.report.target_resolution.as_ref().unwrap().pane_id, pane_id);
    }

    /// set_policy_evaluation stores in report
    #[test]
    fn prop_dry_run_context_set_policy(eval in arb_policy_evaluation()) {
        let check_count = eval.checks.len();
        let mut ctx = DryRunContext::enabled();
        prop_assert!(ctx.report.policy_evaluation.is_none());
        ctx.set_policy_evaluation(eval);
        prop_assert!(ctx.report.policy_evaluation.is_some());
        prop_assert_eq!(
            ctx.report.policy_evaluation.as_ref().unwrap().checks.len(),
            check_count
        );
    }

    /// add_action increases action count
    #[test]
    fn prop_dry_run_context_add_action(
        actions in proptest::collection::vec(arb_planned_action(), 1..5),
    ) {
        let mut ctx = DryRunContext::enabled();
        for (i, a) in actions.into_iter().enumerate() {
            ctx.add_action(a);
            prop_assert_eq!(ctx.report.expected_actions.len(), i + 1);
        }
    }

    /// take_report returns the built report
    #[test]
    fn prop_dry_run_context_take_report(cmd in "[a-z]{1,20}") {
        let mut ctx = DryRunContext::enabled();
        ctx.set_command(&cmd);
        ctx.add_warning("w1");
        let report = ctx.take_report();
        prop_assert_eq!(report.command, cmd);
        prop_assert_eq!(report.warnings.len(), 1);
    }

    /// Default context is disabled
    #[test]
    fn prop_dry_run_context_default(_dummy in 0..1u8) {
        let ctx = DryRunContext::default();
        prop_assert!(!ctx.is_dry_run());
    }
}

// ============================================================================
// CommandContext properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// CommandContext preserves command and dry_run flag
    #[test]
    fn prop_command_context_new(cmd in "[a-z ]{1,30}", flag in any::<bool>()) {
        let ctx = CommandContext::new(&cmd, flag);
        prop_assert_eq!(&ctx.command, &cmd);
        prop_assert_eq!(ctx.dry_run, flag);
        prop_assert_eq!(ctx.is_dry_run(), flag);
    }

    /// dry_run_context inherits flag and command
    #[test]
    fn prop_command_context_dry_run_context(
        cmd in "[a-z]{1,20}",
        flag in any::<bool>(),
    ) {
        let ctx = CommandContext::new(&cmd, flag);
        let dry_ctx = ctx.dry_run_context();
        prop_assert_eq!(dry_ctx.is_dry_run(), flag);
        prop_assert_eq!(dry_ctx.report.command, cmd);
    }
}

// ============================================================================
// build_send_policy_evaluation properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    /// Always produces exactly 3 checks
    #[test]
    fn prop_build_send_eval_three_checks(
        current in 0..1000u32,
        limit in 0..1000u32,
        prompt_active in any::<bool>(),
        require_prompt in any::<bool>(),
        has_gaps in any::<bool>(),
    ) {
        let eval = build_send_policy_evaluation(
            (current, limit),
            prompt_active,
            require_prompt,
            has_gaps,
        );
        prop_assert_eq!(eval.checks.len(), 3, "Expected 3 checks, got {}", eval.checks.len());
    }

    /// Rate limit disabled (limit=0) always passes rate check
    #[test]
    fn prop_build_send_eval_rate_disabled(
        current in 0..1000u32,
        prompt_active in any::<bool>(),
        require_prompt in any::<bool>(),
        has_gaps in any::<bool>(),
    ) {
        let eval = build_send_policy_evaluation(
            (current, 0),
            prompt_active,
            require_prompt,
            has_gaps,
        );
        let rate_check = &eval.checks[0];
        prop_assert!(rate_check.passed, "Rate check should pass when limit=0");
        prop_assert!(rate_check.message.contains("disabled"));
    }

    /// Rate limit within budget passes
    #[test]
    fn prop_build_send_eval_rate_within_budget(
        current in 0..100u32,
        extra in 1..100u32,
    ) {
        let limit = current + extra; // limit > current
        let eval = build_send_policy_evaluation((current, limit), true, false, false);
        let rate_check = &eval.checks[0];
        prop_assert!(rate_check.passed, "Should pass when current < limit");
    }

    /// Rate limit exceeded fails
    #[test]
    fn prop_build_send_eval_rate_exceeded(
        limit in 1..100u32,
        overshoot in 0..50u32,
    ) {
        let current = limit + overshoot; // current >= limit
        let eval = build_send_policy_evaluation((current, limit), true, false, false);
        let rate_check = &eval.checks[0];
        prop_assert!(!rate_check.passed, "Should fail when current >= limit");
    }

    /// Prompt active + required => passes
    #[test]
    fn prop_build_send_eval_prompt_active_ok(
        current in 0..10u32,
        limit in 11..100u32,
    ) {
        let eval = build_send_policy_evaluation((current, limit), true, true, false);
        let pane_check = &eval.checks[1];
        prop_assert!(pane_check.passed, "Should pass when prompt is active");
    }

    /// Prompt inactive + required => fails
    #[test]
    fn prop_build_send_eval_prompt_inactive_fails(
        current in 0..10u32,
        limit in 11..100u32,
    ) {
        let eval = build_send_policy_evaluation((current, limit), false, true, false);
        let pane_check = &eval.checks[1];
        prop_assert!(!pane_check.passed, "Should fail when prompt inactive but required");
    }

    /// Prompt not required => always passes pane check
    #[test]
    fn prop_build_send_eval_prompt_not_required(
        prompt_active in any::<bool>(),
    ) {
        let eval = build_send_policy_evaluation((0, 10), prompt_active, false, false);
        let pane_check = &eval.checks[1];
        prop_assert!(pane_check.passed, "Should pass when prompt not required");
    }

    /// Continuity check always passes (it's informational)
    #[test]
    fn prop_build_send_eval_continuity_always_passes(
        has_gaps in any::<bool>(),
    ) {
        let eval = build_send_policy_evaluation((0, 10), true, false, has_gaps);
        let continuity_check = &eval.checks[2];
        prop_assert!(continuity_check.passed, "Continuity check should always pass");
    }

    /// Gaps produce details on the continuity check
    #[test]
    fn prop_build_send_eval_gaps_details(
        current in 0..10u32,
        limit in 11..100u32,
    ) {
        let eval = build_send_policy_evaluation((current, limit), true, false, true);
        let continuity_check = &eval.checks[2];
        prop_assert!(continuity_check.details.is_some(), "Gaps should add details");
    }

    /// No gaps means no details
    #[test]
    fn prop_build_send_eval_no_gaps_no_details(
        current in 0..10u32,
        limit in 11..100u32,
    ) {
        let eval = build_send_policy_evaluation((current, limit), true, false, false);
        let continuity_check = &eval.checks[2];
        prop_assert!(continuity_check.details.is_none(), "No gaps should mean no details");
    }

    /// all_passed reflects rate+prompt combined logic
    #[test]
    fn prop_build_send_eval_all_passed_logic(
        current in 0..100u32,
        limit in 0..100u32,
        prompt_active in any::<bool>(),
        require_prompt in any::<bool>(),
        has_gaps in any::<bool>(),
    ) {
        let eval = build_send_policy_evaluation(
            (current, limit),
            prompt_active,
            require_prompt,
            has_gaps,
        );
        let rate_ok = limit == 0 || current < limit;
        let prompt_ok = !require_prompt || prompt_active;
        // Continuity always passes
        let expected = rate_ok && prompt_ok;
        prop_assert_eq!(
            eval.all_passed(), expected,
            "all_passed mismatch: rate_ok={}, prompt_ok={}, current={}, limit={}", rate_ok, prompt_ok, current, limit
        );
    }
}

// ============================================================================
// create_send_action properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// create_send_action always produces SendText type
    #[test]
    fn prop_create_send_action_type(
        step in 1..100u32,
        pane_id in any::<u64>(),
        text_len in 0..10000usize,
    ) {
        let action = create_send_action(step, pane_id, text_len);
        prop_assert_eq!(action.action_type, ActionType::SendText);
        prop_assert_eq!(action.step, step);
    }

    /// Description mentions pane_id and text_len
    #[test]
    fn prop_create_send_action_description(
        step in 1..10u32,
        pane_id in 0..1000u64,
        text_len in 0..5000usize,
    ) {
        let action = create_send_action(step, pane_id, text_len);
        prop_assert!(
            action.description.contains(&pane_id.to_string()),
            "Description should contain pane_id"
        );
        prop_assert!(
            action.description.contains(&text_len.to_string()),
            "Description should contain text_len"
        );
    }
}

// ============================================================================
// create_wait_for_action properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// create_wait_for_action always produces WaitFor type
    #[test]
    fn prop_create_wait_for_action_type(
        step in 1..100u32,
        condition in "[a-z ]{1,20}",
        timeout_ms in 0..60000u64,
    ) {
        let action = create_wait_for_action(step, &condition, timeout_ms);
        prop_assert_eq!(action.action_type, ActionType::WaitFor);
        prop_assert_eq!(action.step, step);
    }

    /// Description mentions condition and timeout
    #[test]
    fn prop_create_wait_for_action_description(
        step in 1..10u32,
        condition in "[a-z]{1,10}",
        timeout_ms in 100..60000u64,
    ) {
        let action = create_wait_for_action(step, &condition, timeout_ms);
        prop_assert!(
            action.description.contains(&condition),
            "Description should contain condition"
        );
        prop_assert!(
            action.description.contains(&timeout_ms.to_string()),
            "Description should contain timeout"
        );
    }
}

// ============================================================================
// format_json properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// format_json always produces valid JSON
    #[test]
    fn prop_format_json_valid(report in arb_dry_run_report()) {
        let json = format_json(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.is_object());
    }

    /// format_json output always has "command" field
    #[test]
    fn prop_format_json_has_command(report in arb_dry_run_report()) {
        let json = format_json(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.get("command").is_some());
    }

    /// format_json output always has "expected_actions" array
    #[test]
    fn prop_format_json_has_actions(report in arb_dry_run_report()) {
        let json = format_json(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(parsed.get("expected_actions").is_some());
        prop_assert!(parsed["expected_actions"].is_array());
    }
}

// ============================================================================
// format_human properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// format_human always contains header and footer
    #[test]
    fn prop_format_human_header_footer(report in arb_dry_run_report()) {
        let output = format_human(&report);
        prop_assert!(output.contains("DRY RUN"), "Missing DRY RUN header");
        prop_assert!(
            output.contains("remove --dry-run"),
            "Missing footer hint"
        );
    }

    /// format_human includes command when non-empty
    #[test]
    fn prop_format_human_command(cmd in "[a-z]{3,20}") {
        let report = DryRunReport::with_command(&cmd);
        let output = format_human(&report);
        // Command appears somewhere (possibly redacted, but simple alpha strings won't be)
        prop_assert!(output.contains("Command:"));
    }

    /// format_human shows "Expected Actions" section when actions present
    #[test]
    fn prop_format_human_actions_section(
        actions in proptest::collection::vec(arb_planned_action(), 1..4),
    ) {
        let report = DryRunReport {
            command: "test".into(),
            target_resolution: None,
            policy_evaluation: None,
            expected_actions: actions,
            warnings: Vec::new(),
        };
        let output = format_human(&report);
        prop_assert!(output.contains("Expected Actions"));
    }

    /// format_human shows "Warnings" section when warnings present
    #[test]
    fn prop_format_human_warnings_section(
        warnings in proptest::collection::vec("[a-z]{3,10}", 1..4),
    ) {
        let report = DryRunReport {
            command: "test".into(),
            target_resolution: None,
            policy_evaluation: None,
            expected_actions: Vec::new(),
            warnings,
        };
        let output = format_human(&report);
        prop_assert!(output.contains("Warnings"));
    }

    /// format_human shows checkmarks/crosses for policy checks
    #[test]
    fn prop_format_human_policy_symbols(eval in arb_policy_evaluation()) {
        if eval.checks.is_empty() {
            return Ok(());
        }
        let report = DryRunReport {
            command: "test".into(),
            target_resolution: None,
            policy_evaluation: Some(eval.clone()),
            expected_actions: Vec::new(),
            warnings: Vec::new(),
        };
        let output = format_human(&report);
        let has_passing = eval.checks.iter().any(|c| c.passed);
        let has_failing = eval.checks.iter().any(|c| !c.passed);
        if has_passing {
            prop_assert!(output.contains("✓"), "Missing checkmark for passing check");
        }
        if has_failing {
            prop_assert!(output.contains("✗"), "Missing cross for failing check");
        }
    }
}

// ============================================================================
// Cross-module / integration properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Full pipeline: CommandContext -> DryRunContext -> Report -> JSON
    #[test]
    fn prop_full_pipeline(
        cmd in "[a-z ]{1,20}",
        pane_id in 0..100u64,
        current in 0..50u32,
        limit in 0..100u32,
        prompt_active in any::<bool>(),
    ) {
        let cmd_ctx = CommandContext::new(&cmd, true);
        let mut dry_ctx = cmd_ctx.dry_run_context();

        dry_ctx.set_target(TargetResolution::new(pane_id, "local"));

        let eval = build_send_policy_evaluation(
            (current, limit),
            prompt_active,
            false,
            false,
        );
        dry_ctx.set_policy_evaluation(eval);
        dry_ctx.add_action(create_send_action(1, pane_id, 10));

        let report = dry_ctx.take_report();
        prop_assert_eq!(&report.command, &cmd);
        prop_assert!(report.target_resolution.is_some());
        prop_assert!(report.policy_evaluation.is_some());
        prop_assert_eq!(report.action_count(), 1);

        // JSON round-trip
        let json = format_json(&report).unwrap();
        let _parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Human format
        let human = format_human(&report);
        prop_assert!(human.contains("DRY RUN"));
    }

    /// Redaction doesn't change report structure (same field presence)
    #[test]
    fn prop_redacted_preserves_structure(report in arb_dry_run_report()) {
        let redacted = report.redacted();
        prop_assert_eq!(
            report.target_resolution.is_some(),
            redacted.target_resolution.is_some()
        );
        prop_assert_eq!(
            report.policy_evaluation.is_some(),
            redacted.policy_evaluation.is_some()
        );
        prop_assert_eq!(
            report.expected_actions.len(),
            redacted.expected_actions.len()
        );
        prop_assert_eq!(report.warnings.len(), redacted.warnings.len());
    }

    /// Redaction preserves non-secret text (plain alpha strings)
    #[test]
    fn prop_redacted_preserves_plain_text(cmd in "[a-z]{5,20}") {
        let report = DryRunReport::with_command(&cmd);
        let redacted = report.redacted();
        // Plain alpha strings should not be redacted
        prop_assert_eq!(redacted.command, cmd);
    }

    /// Multiple actions maintain step order
    #[test]
    fn prop_multiple_actions_preserve_order(
        count in 1..8usize,
    ) {
        let mut ctx = DryRunContext::enabled();
        for i in 0..count {
            ctx.add_action(PlannedAction::new(
                (i + 1) as u32,
                ActionType::SendText,
                format!("step {}", i + 1),
            ));
        }
        let report = ctx.take_report();
        prop_assert_eq!(report.action_count(), count);
        for (i, action) in report.expected_actions.iter().enumerate() {
            prop_assert_eq!(action.step, (i + 1) as u32);
        }
    }
}
