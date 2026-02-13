//! Property-based tests for workflows module.
//!
//! Verifies workflow engine invariants:
//! - StepResult: serde roundtrip, is_terminal/is_done/is_continue consistency
//! - TextMatch/WaitCondition: serde roundtrip, pane_id extraction
//! - PaneWorkflowLockManager: mutual exclusion, release correctness, guard RAII
//! - BroadcastPrecondition: check consistency with PaneCapabilities
//! - BroadcastResult: count invariants (allowed+denied+precond+skipped == total)
//! - validate_session_id: hex+hyphen acceptance, short rejection
//! - FallbackNextStepPlan: builder version, serde roundtrip, is_fallback_result
//! - WorkflowDescriptor: validation rejects bad schemas, empty steps, duplicate ids
//! - DescriptorFailureHandler: interpolation correctness
//! - ResumeSessionConfig: format_resume_command substitution

use proptest::prelude::*;

use frankenterm_core::policy::PaneCapabilities;
use frankenterm_core::workflows::{
    BroadcastPrecondition, BroadcastResult, DescriptorFailureHandler, FallbackNextStepPlan,
    FallbackReason, LockAcquisitionResult, PaneBroadcastOutcome, PaneGroupStrategy,
    PaneWorkflowLockManager, ResumeSessionConfig, StepResult, TextMatch, WaitCondition,
    WorkflowDescriptor, build_all_accounts_exhausted_plan, build_failover_disabled_plan,
    build_needs_human_auth_plan, build_tool_missing_plan, check_preconditions,
    default_broadcast_preconditions, fallback_plan_to_step_result, format_resume_command,
    is_fallback_result, validate_session_id,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_text_match() -> impl Strategy<Value = TextMatch> {
    prop_oneof![
        "[a-zA-Z0-9_ ]{1,64}".prop_map(TextMatch::substring),
        Just(TextMatch::regex("[a-z]+".to_string())),
    ]
}

fn arb_wait_condition() -> impl Strategy<Value = WaitCondition> {
    prop_oneof![
        "[a-z_]{1,32}".prop_map(|rule| WaitCondition::pattern(rule)),
        (1u64..=60_000).prop_map(WaitCondition::pane_idle),
        (1u64..=60_000).prop_map(WaitCondition::stable_tail),
        arb_text_match().prop_map(WaitCondition::text_match),
        (1u64..=30_000).prop_map(WaitCondition::sleep),
        "[a-z_]{1,16}".prop_map(|key| WaitCondition::external(key)),
    ]
}

fn arb_step_result() -> impl Strategy<Value = StepResult> {
    prop_oneof![
        Just(StepResult::cont()),
        Just(StepResult::done_empty()),
        (1u64..=10_000).prop_map(StepResult::retry),
        "[a-zA-Z0-9 ]{1,64}".prop_map(|r| StepResult::abort(r)),
        arb_wait_condition().prop_map(StepResult::wait_for),
        "[a-zA-Z0-9 ]{1,64}".prop_map(|t| StepResult::send_text(t)),
    ]
}

fn arb_pane_capabilities() -> impl Strategy<Value = PaneCapabilities> {
    (
        any::<bool>(),
        any::<bool>(),
        any::<Option<bool>>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(prompt, cmd, alt, gap, reserved)| PaneCapabilities {
            prompt_active: prompt,
            command_running: cmd,
            alt_screen: alt,
            has_recent_gap: gap,
            is_reserved: reserved,
            reserved_by: None,
        })
}

fn arb_fallback_reason() -> impl Strategy<Value = FallbackReason> {
    prop_oneof![
        ("[a-z]{3,10}", "[a-z ]{5,30}").prop_map(|(acct, detail)| {
            FallbackReason::NeedsHumanAuth {
                account: acct,
                detail,
            }
        }),
        Just(FallbackReason::FailoverDisabled),
        "[a-z_]{3,10}".prop_map(|tool| FallbackReason::ToolMissing { tool }),
        "[a-z_]{3,20}".prop_map(|rule| FallbackReason::PolicyDenied { rule }),
        (1u32..=50).prop_map(|n| FallbackReason::AllAccountsExhausted {
            accounts_checked: n,
        }),
        "[a-z ]{5,30}".prop_map(|detail| FallbackReason::Other { detail }),
    ]
}

fn arb_broadcast_precondition() -> impl Strategy<Value = BroadcastPrecondition> {
    prop_oneof![
        Just(BroadcastPrecondition::PromptActive),
        Just(BroadcastPrecondition::NotAltScreen),
        Just(BroadcastPrecondition::NoRecentGap),
        Just(BroadcastPrecondition::NotReserved),
    ]
}

// ────────────────────────────────────────────────────────────────────
// StepResult properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn step_result_serde_roundtrip(sr in arb_step_result()) {
        let json = serde_json::to_string(&sr).unwrap();
        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        // Verify structural equivalence via re-serialization
        let json2 = serde_json::to_string(&parsed).unwrap();
        prop_assert_eq!(&json, &json2, "serde roundtrip mismatch");
    }

    #[test]
    fn step_result_is_terminal_iff_done_or_abort(sr in arb_step_result()) {
        let terminal = sr.is_terminal();
        let is_done = sr.is_done();
        let is_abort = matches!(sr, StepResult::Abort { .. });
        prop_assert_eq!(terminal, is_done || is_abort,
            "is_terminal should equal is_done || is_abort");
    }

    #[test]
    fn step_result_is_continue_exclusive(sr in arb_step_result()) {
        if sr.is_continue() {
            prop_assert!(!sr.is_done(), "Continue should not be Done");
            prop_assert!(!sr.is_terminal(), "Continue should not be terminal");
            prop_assert!(!sr.is_send_text(), "Continue should not be SendText");
        }
    }

    #[test]
    fn step_result_done_is_terminal(sr in arb_step_result()) {
        if sr.is_done() {
            prop_assert!(sr.is_terminal(), "Done should be terminal");
            prop_assert!(!sr.is_continue(), "Done should not be Continue");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// TextMatch / WaitCondition properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn text_match_serde_roundtrip(tm in arb_text_match()) {
        let json = serde_json::to_string(&tm).unwrap();
        let parsed: TextMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&tm, &parsed, "TextMatch serde roundtrip failed");
    }

    #[test]
    fn wait_condition_serde_roundtrip(wc in arb_wait_condition()) {
        let json = serde_json::to_string(&wc).unwrap();
        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&wc, &parsed, "WaitCondition serde roundtrip failed");
    }

    #[test]
    fn wait_condition_pane_id_consistency(wc in arb_wait_condition()) {
        let pid = wc.pane_id();
        match &wc {
            WaitCondition::Pattern { pane_id, .. }
            | WaitCondition::PaneIdle { pane_id, .. }
            | WaitCondition::StableTail { pane_id, .. }
            | WaitCondition::TextMatch { pane_id, .. } => {
                prop_assert_eq!(pid, *pane_id,
                    "pane_id() should match inner pane_id");
            }
            WaitCondition::Sleep { .. } | WaitCondition::External { .. } => {
                prop_assert!(pid.is_none(),
                    "Sleep/External should have no pane_id");
            }
        }
    }

    #[test]
    fn wait_condition_with_pane_id_returns_some(
        pane_id in 1u64..1000,
        idle_ms in 100u64..5000
    ) {
        let wc = WaitCondition::pane_idle_on(pane_id, idle_ms);
        prop_assert_eq!(wc.pane_id(), Some(pane_id));
    }
}

// ────────────────────────────────────────────────────────────────────
// PaneWorkflowLockManager properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lock_acquire_then_release(
        pane_id in 0u64..100,
        workflow in "[a-z]{3,10}",
        exec_id in "[a-z0-9]{8,16}"
    ) {
        let mgr = PaneWorkflowLockManager::new();

        // Acquire should succeed
        let result = mgr.try_acquire(pane_id, &workflow, &exec_id);
        prop_assert!(result.is_acquired(), "First acquire should succeed");
        prop_assert!(mgr.is_locked(pane_id).is_some(), "Pane should be locked");

        // Release with correct exec_id should succeed
        let released = mgr.release(pane_id, &exec_id);
        prop_assert!(released, "Release with correct exec_id should succeed");
        prop_assert!(mgr.is_locked(pane_id).is_none(), "Pane should be unlocked");
    }

    #[test]
    fn lock_mutual_exclusion(
        pane_id in 0u64..100,
        wf1 in "[a-z]{3,10}",
        wf2 in "[a-z]{3,10}",
        exec1 in "[a-z0-9]{8}",
        exec2 in "[a-z0-9]{8}"
    ) {
        let mgr = PaneWorkflowLockManager::new();

        let r1 = mgr.try_acquire(pane_id, &wf1, &exec1);
        prop_assert!(r1.is_acquired(), "First acquire should succeed");

        // Second acquire on same pane should fail
        let r2 = mgr.try_acquire(pane_id, &wf2, &exec2);
        prop_assert!(r2.is_already_locked(),
            "Second acquire on locked pane should fail");

        if let LockAcquisitionResult::AlreadyLocked { held_by_workflow, held_by_execution, .. } = &r2 {
            prop_assert_eq!(held_by_workflow, &wf1,
                "Held-by workflow should match first acquirer");
            prop_assert_eq!(held_by_execution, &exec1,
                "Held-by execution should match first acquirer");
        }
    }

    #[test]
    fn lock_release_wrong_exec_id_fails(
        pane_id in 0u64..100,
        workflow in "[a-z]{3,10}",
        exec1 in "[a-z]{8}",
        exec2 in "[A-Z]{8}"  // guaranteed different from exec1
    ) {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(pane_id, &workflow, &exec1);

        // Release with wrong exec_id should fail
        let released = mgr.release(pane_id, &exec2);
        prop_assert!(!released, "Release with wrong exec_id should fail");
        prop_assert!(mgr.is_locked(pane_id).is_some(), "Lock should still be held");
    }

    #[test]
    fn lock_guard_releases_on_drop(
        pane_id in 0u64..100,
        workflow in "[a-z]{3,10}",
        exec_id in "[a-z0-9]{8,16}"
    ) {
        let mgr = PaneWorkflowLockManager::new();

        {
            let guard = mgr.acquire_guard(pane_id, &workflow, &exec_id);
            prop_assert!(guard.is_some(), "Guard should be acquired");
            prop_assert!(mgr.is_locked(pane_id).is_some(), "Should be locked");
            // guard drops here
        }

        prop_assert!(mgr.is_locked(pane_id).is_none(),
            "Lock should be released after guard drop");
    }

    #[test]
    fn lock_active_count_matches_acquisitions(
        pane_ids in prop::collection::hash_set(0u64..100, 1..10)
    ) {
        let mgr = PaneWorkflowLockManager::new();

        for (i, &pid) in pane_ids.iter().enumerate() {
            mgr.try_acquire(pid, "wf", &format!("exec-{}", i));
        }

        let active = mgr.active_locks();
        prop_assert_eq!(active.len(), pane_ids.len(),
            "Active lock count should match unique pane acquisitions");
    }

    #[test]
    fn force_release_always_succeeds(
        pane_id in 0u64..100,
        workflow in "[a-z]{3,10}",
        exec_id in "[a-z0-9]{8}"
    ) {
        let mgr = PaneWorkflowLockManager::new();
        mgr.try_acquire(pane_id, &workflow, &exec_id);

        let removed = mgr.force_release(pane_id);
        prop_assert!(removed.is_some(), "Force release should return the lock info");
        prop_assert!(mgr.is_locked(pane_id).is_none(), "Pane should be unlocked");
    }
}

// ────────────────────────────────────────────────────────────────────
// BroadcastPrecondition properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn precondition_check_matches_manual(
        caps in arb_pane_capabilities(),
        precond in arb_broadcast_precondition()
    ) {
        let result = precond.check(&caps);
        let expected = match &precond {
            BroadcastPrecondition::PromptActive => caps.prompt_active,
            BroadcastPrecondition::NotAltScreen => !caps.alt_screen.unwrap_or(false),
            BroadcastPrecondition::NoRecentGap => !caps.has_recent_gap,
            BroadcastPrecondition::NotReserved => !caps.is_reserved,
        };
        prop_assert_eq!(result, expected,
            "Precondition check should match manual computation");
    }

    #[test]
    fn check_preconditions_failures_subset_of_preconditions(
        caps in arb_pane_capabilities()
    ) {
        let preconditions = default_broadcast_preconditions();
        let failures = check_preconditions(&preconditions, &caps);

        // Every failure label should be a valid precondition label
        let valid_labels: Vec<&str> = preconditions.iter().map(|p| p.label()).collect();
        for f in &failures {
            prop_assert!(valid_labels.contains(f),
                "Failure label {} not in valid labels", f);
        }

        // Number of failures + passes should equal total preconditions
        let pass_count = preconditions.iter().filter(|p| p.check(&caps)).count();
        prop_assert_eq!(failures.len() + pass_count, preconditions.len(),
            "Failures + passes should equal total preconditions");
    }

    #[test]
    fn all_preconditions_pass_when_ideal_caps(
        _dummy in 0..1u8  // just run once
    ) {
        let caps = PaneCapabilities {
            prompt_active: true,
            command_running: false,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        };
        let failures = check_preconditions(&default_broadcast_preconditions(), &caps);
        prop_assert!(failures.is_empty(), "Ideal caps should pass all preconditions");
    }
}

// ────────────────────────────────────────────────────────────────────
// BroadcastResult counting properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn broadcast_result_counts_sum_to_total(
        outcomes in prop::collection::vec(
            prop_oneof![
                (1u64..=10000).prop_map(|ms| PaneBroadcastOutcome::Allowed { elapsed_ms: ms }),
                "[a-z ]{3,20}".prop_map(|r| PaneBroadcastOutcome::Denied { reason: r }),
                prop::collection::vec("[a-z]{3,10}", 1..3)
                    .prop_map(|f| PaneBroadcastOutcome::PreconditionFailed { failed: f }),
                "[a-z ]{3,20}".prop_map(|r| PaneBroadcastOutcome::Skipped { reason: r }),
            ],
            0..20
        )
    ) {
        let mut result = BroadcastResult::new("test_action");
        for (i, outcome) in outcomes.iter().enumerate() {
            result.add_outcome(i as u64, outcome.clone());
        }

        let total = result.outcomes.len();
        let sum = result.allowed_count()
            + result.denied_count()
            + result.precondition_failed_count()
            + result.skipped_count();

        prop_assert_eq!(sum, total,
            "allowed+denied+precond+skipped should equal total outcomes");
    }

    #[test]
    fn broadcast_all_allowed_iff_all_outcomes_allowed(
        count in 1usize..=10
    ) {
        let mut result = BroadcastResult::new("test");
        for i in 0..count {
            result.add_outcome(i as u64, PaneBroadcastOutcome::Allowed { elapsed_ms: 100 });
        }
        prop_assert!(result.all_allowed(), "All-allowed result should report all_allowed");

        // Add one denied → no longer all_allowed
        result.add_outcome(count as u64, PaneBroadcastOutcome::Denied { reason: "no".into() });
        prop_assert!(!result.all_allowed(), "Mixed result should not be all_allowed");
    }

    #[test]
    fn broadcast_empty_not_all_allowed(_dummy in 0..1u8) {
        let result = BroadcastResult::new("test");
        prop_assert!(!result.all_allowed(), "Empty result should not be all_allowed");
    }
}

// ────────────────────────────────────────────────────────────────────
// validate_session_id properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn valid_hex_session_ids_accepted(
        hex_str in "[0-9a-fA-F]{8,36}"
    ) {
        prop_assert!(validate_session_id(&hex_str),
            "Pure hex string of length >= 8 should be valid");
    }

    #[test]
    fn valid_uuid_format_accepted(
        a in "[0-9a-f]{8}",
        b in "[0-9a-f]{4}",
        c in "[0-9a-f]{4}",
        d in "[0-9a-f]{4}",
        e in "[0-9a-f]{12}"
    ) {
        let uuid = format!("{}-{}-{}-{}-{}", a, b, c, d, e);
        prop_assert!(validate_session_id(&uuid),
            "UUID-format string should be valid");
    }

    #[test]
    fn short_strings_rejected(s in "[0-9a-f]{1,7}") {
        prop_assert!(!validate_session_id(&s),
            "String shorter than 8 chars should be rejected");
    }

    #[test]
    fn non_hex_strings_rejected(s in "[g-z]{8,20}") {
        prop_assert!(!validate_session_id(&s),
            "Non-hex string should be rejected");
    }
}

// ────────────────────────────────────────────────────────────────────
// FallbackNextStepPlan properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn fallback_plan_serde_roundtrip(
        reason in arb_fallback_reason(),
        pane_id in 0u64..1000,
        now_ms in 1_000_000i64..2_000_000_000
    ) {
        let plan = FallbackNextStepPlan {
            version: FallbackNextStepPlan::CURRENT_VERSION,
            reason,
            pane_id,
            operator_steps: vec!["Step 1".into(), "Step 2".into()],
            retry_after_ms: Some(now_ms + 60_000),
            resume_session_id: Some("abc12345".into()),
            account_id: Some("test-account".into()),
            suggested_commands: vec!["ft status".into()],
            created_at_ms: now_ms,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&parsed).unwrap();
        prop_assert_eq!(&json, &json2, "FallbackNextStepPlan serde roundtrip failed");
    }

    #[test]
    fn fallback_plan_to_step_is_done_and_fallback(
        pane_id in 0u64..1000,
        now_ms in 1_000_000i64..2_000_000_000
    ) {
        let plan = build_failover_disabled_plan(pane_id, None, None, now_ms);
        let step = fallback_plan_to_step_result(&plan);

        prop_assert!(step.is_done(), "Fallback plan should produce Done result");
        prop_assert!(step.is_terminal(), "Fallback plan should be terminal");
        prop_assert!(is_fallback_result(&step),
            "Fallback plan result should be identified as fallback");
    }

    #[test]
    fn non_fallback_done_not_detected_as_fallback(
        val in prop_oneof![
            Just(serde_json::Value::Null),
            Just(serde_json::json!({"key": "value"})),
            Just(serde_json::json!(42)),
        ]
    ) {
        let step = StepResult::Done { result: val };
        prop_assert!(!is_fallback_result(&step),
            "Non-fallback Done should not be detected as fallback");
    }

    #[test]
    fn needs_human_auth_plan_has_correct_version(
        pane_id in 0u64..100,
        account in "[a-z]{3,10}",
        now_ms in 1_000_000i64..2_000_000_000
    ) {
        let plan = build_needs_human_auth_plan(pane_id, &account, "test detail", None, None, now_ms);
        prop_assert_eq!(plan.version, FallbackNextStepPlan::CURRENT_VERSION,
            "Plan version should match CURRENT_VERSION");
        prop_assert_eq!(plan.pane_id, pane_id, "Plan pane_id should match");
        prop_assert!(!plan.operator_steps.is_empty(),
            "Needs-human-auth plan should have operator steps");
    }

    #[test]
    fn tool_missing_plan_has_correct_structure(
        pane_id in 0u64..100,
        tool in "[a-z_]{3,10}",
        now_ms in 1_000_000i64..2_000_000_000
    ) {
        let plan = build_tool_missing_plan(pane_id, &tool, now_ms);
        prop_assert_eq!(plan.version, FallbackNextStepPlan::CURRENT_VERSION);
        prop_assert!(plan.retry_after_ms.is_none(),
            "Tool-missing plan should have no retry_after");
        prop_assert!(plan.resume_session_id.is_none(),
            "Tool-missing plan should have no resume session");
        prop_assert!(plan.operator_steps.iter().any(|s| s.contains(&tool)),
            "Tool-missing plan operator steps should mention the tool");
    }

    #[test]
    fn all_accounts_exhausted_plan_mentions_count(
        pane_id in 0u64..100,
        count in 1u32..=20,
        now_ms in 1_000_000i64..2_000_000_000
    ) {
        let plan = build_all_accounts_exhausted_plan(pane_id, count, None, None, now_ms);
        prop_assert_eq!(plan.version, FallbackNextStepPlan::CURRENT_VERSION);
        if let FallbackReason::AllAccountsExhausted { accounts_checked } = &plan.reason {
            prop_assert_eq!(*accounts_checked, count,
                "accounts_checked should match input");
        } else {
            prop_assert!(false, "Expected AllAccountsExhausted reason");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DescriptorFailureHandler interpolation
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn failure_handler_interpolates_step_name(
        step_name in "[a-z_]{3,20}",
        prefix in "[a-zA-Z ]{3,20}"
    ) {
        let handler = DescriptorFailureHandler::Notify {
            message: format!("{} ${{failed_step}} happened", prefix),
        };
        let result = handler.interpolate_message(&step_name);
        prop_assert!(result.contains(&step_name),
            "Interpolated message should contain step name");
        prop_assert!(!result.contains("${failed_step}"),
            "Interpolated message should not contain placeholder");
    }

    #[test]
    fn failure_handler_no_placeholder_is_noop(
        message in "[a-zA-Z ]{5,30}",
        step_name in "[a-z_]{3,10}"
    ) {
        // Message without ${failed_step} should be returned as-is
        let handler = DescriptorFailureHandler::Log { message: message.clone() };
        let result = handler.interpolate_message(&step_name);
        prop_assert_eq!(&result, &message,
            "Message without placeholder should be returned unchanged");
    }
}

// ────────────────────────────────────────────────────────────────────
// ResumeSessionConfig properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn format_resume_command_substitutes_session_id(
        session_id in "[0-9a-f]{8,32}"
    ) {
        let config = ResumeSessionConfig::default();
        let cmd = format_resume_command(&session_id, &config);
        prop_assert!(cmd.contains(&session_id),
            "Resume command should contain session_id");
        prop_assert!(!cmd.contains("{session_id}"),
            "Resume command should not contain raw placeholder");
    }

    #[test]
    fn format_resume_command_custom_template(
        session_id in "[0-9a-f]{8}",
        prefix in "[a-z]{3,10}"
    ) {
        let config = ResumeSessionConfig {
            resume_command_template: format!("{} {{session_id}} --flag\n", prefix),
            ..Default::default()
        };
        let cmd = format_resume_command(&session_id, &config);
        prop_assert!(cmd.starts_with(&prefix),
            "Command should start with custom prefix");
        prop_assert!(cmd.contains(&session_id),
            "Command should contain session_id");
    }
}

// ────────────────────────────────────────────────────────────────────
// WorkflowDescriptor validation properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn descriptor_rejects_wrong_schema_version(version in 2u32..=100) {
        let yaml = format!(r#"
workflow_schema_version: {}
name: test
steps:
  - type: sleep
    id: step1
    duration_ms: 1000
"#, version);
        let result = WorkflowDescriptor::from_yaml_str(&yaml);
        prop_assert!(result.is_err(),
            "Descriptor with wrong schema version should be rejected");
    }

    #[test]
    fn descriptor_rejects_empty_steps(_dummy in 0..1u8) {
        let yaml = r#"
workflow_schema_version: 1
name: test
steps: []
"#;
        let result = WorkflowDescriptor::from_yaml_str(yaml);
        prop_assert!(result.is_err(),
            "Descriptor with empty steps should be rejected");
    }

    #[test]
    fn descriptor_rejects_too_many_steps(n in 33usize..=50) {
        let mut steps = String::new();
        for i in 0..n {
            steps.push_str(&format!("  - type: sleep\n    id: step{}\n    duration_ms: 100\n", i));
        }
        let yaml = format!(
            "workflow_schema_version: 1\nname: test\nsteps:\n{}", steps
        );
        let result = WorkflowDescriptor::from_yaml_str(&yaml);
        prop_assert!(result.is_err(),
            "Descriptor with {} steps should exceed max (32)", n);
    }

    #[test]
    fn descriptor_valid_single_sleep_step(duration_ms in 1u64..=30_000) {
        let yaml = format!(r#"
workflow_schema_version: 1
name: test_workflow
steps:
  - type: sleep
    id: step1
    duration_ms: {}
"#, duration_ms);
        let result = WorkflowDescriptor::from_yaml_str(&yaml);
        prop_assert!(result.is_ok(),
            "Valid single-sleep descriptor should parse: {:?}", result.err());
    }

    #[test]
    fn descriptor_rejects_sleep_too_long(duration_ms in 30_001u64..=100_000) {
        let yaml = format!(r#"
workflow_schema_version: 1
name: test
steps:
  - type: sleep
    id: step1
    duration_ms: {}
"#, duration_ms);
        let result = WorkflowDescriptor::from_yaml_str(&yaml);
        prop_assert!(result.is_err(),
            "Sleep duration {} should exceed max (30000)", duration_ms);
    }

    #[test]
    fn descriptor_rejects_duplicate_step_ids(id in "[a-z]{3,10}") {
        let yaml = format!(r#"
workflow_schema_version: 1
name: test
steps:
  - type: sleep
    id: {}
    duration_ms: 100
  - type: sleep
    id: {}
    duration_ms: 200
"#, id, id);
        let result = WorkflowDescriptor::from_yaml_str(&yaml);
        prop_assert!(result.is_err(),
            "Descriptor with duplicate step ids should be rejected");
    }
}

// ────────────────────────────────────────────────────────────────────
// PaneGroupStrategy serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_group_strategy_serde_roundtrip(
        strategy in prop_oneof![
            Just(PaneGroupStrategy::ByDomain),
            Just(PaneGroupStrategy::ByAgent),
            Just(PaneGroupStrategy::ByProject),
            prop::collection::vec(0u64..100, 0..5)
                .prop_map(|ids| PaneGroupStrategy::Explicit { pane_ids: ids }),
        ]
    ) {
        let json = serde_json::to_string(&strategy).unwrap();
        let parsed: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&strategy, &parsed,
            "PaneGroupStrategy serde roundtrip failed");
    }
}

// ────────────────────────────────────────────────────────────────────
// FallbackReason Display consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn fallback_reason_display_is_nonempty(reason in arb_fallback_reason()) {
        let display = format!("{}", reason);
        prop_assert!(!display.is_empty(),
            "FallbackReason Display should never be empty");
    }

    #[test]
    fn fallback_reason_serde_roundtrip(reason in arb_fallback_reason()) {
        let json = serde_json::to_string(&reason).unwrap();
        let parsed: FallbackReason = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&parsed).unwrap();
        prop_assert_eq!(&json, &json2,
            "FallbackReason serde roundtrip failed");
    }
}
