//! Property-based tests for plan module invariants.
//!
//! Validates:
//! 1. canonical_string() determinism/idempotency for ALL types
//! 2. compute_hash() determinism for identical plan content
//! 3. compute_hash() excludes created_at and metadata
//! 4. compute_hash() includes workspace_id, title, steps
//! 5. Serde roundtrip for all enum types and structs
//! 6. PlanId::from_hash strips sha256: prefix
//! 7. PlanId::placeholder is recognized by is_placeholder
//! 8. IdempotencyKey::for_action determinism and collision freedom
//! 9. StepPlan::new sets default values correctly
//! 10. ActionPlanBuilder builds with correct version and hash
//! 11. validate() passes/fails correctly
//! 12. action_type_name() returns correct strings
//! 13. OnFailure/Verification factory methods produce expected variants

use proptest::prelude::*;
use serde_json;

use frankenterm_core::plan::*;

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary non-empty string (1..64 chars, printable ASCII no pipes/commas).
fn arb_name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,32}"
}

fn arb_pane_id() -> impl Strategy<Value = u64> {
    0u64..1000
}

fn arb_timeout() -> impl Strategy<Value = u64> {
    100u64..120_000
}

fn arb_step_number() -> impl Strategy<Value = u32> {
    1u32..100
}

/// Generate an arbitrary WaitCondition.
fn arb_wait_condition() -> impl Strategy<Value = WaitCondition> {
    prop_oneof![
        (prop::option::of(arb_pane_id()), arb_name())
            .prop_map(|(pane_id, rule_id)| WaitCondition::Pattern { pane_id, rule_id }),
        (prop::option::of(arb_pane_id()), arb_timeout()).prop_map(
            |(pane_id, idle_threshold_ms)| WaitCondition::PaneIdle {
                pane_id,
                idle_threshold_ms,
            }
        ),
        (prop::option::of(arb_pane_id()), arb_timeout()).prop_map(|(pane_id, stable_for_ms)| {
            WaitCondition::StableTail {
                pane_id,
                stable_for_ms,
            }
        }),
        arb_name().prop_map(|key| WaitCondition::External { key }),
    ]
}

/// Generate an arbitrary StepAction (excluding NestedPlan to avoid recursion).
fn arb_step_action() -> impl Strategy<Value = StepAction> {
    prop_oneof![
        (arb_pane_id(), arb_name(), prop::option::of(any::<bool>())).prop_map(
            |(pane_id, text, paste_mode)| StepAction::SendText {
                pane_id,
                text,
                paste_mode,
            }
        ),
        (
            prop::option::of(arb_pane_id()),
            arb_wait_condition(),
            arb_timeout()
        )
            .prop_map(|(pane_id, condition, timeout_ms)| StepAction::WaitFor {
                pane_id,
                condition,
                timeout_ms,
            }),
        (arb_name(), prop::option::of(arb_timeout())).prop_map(|(lock_name, timeout_ms)| {
            StepAction::AcquireLock {
                lock_name,
                timeout_ms,
            }
        }),
        arb_name().prop_map(|lock_name| StepAction::ReleaseLock { lock_name }),
        arb_name().prop_map(|key| StepAction::StoreData {
            key,
            value: serde_json::json!({"test": true}),
        }),
        (arb_name(), any::<bool>()).prop_map(|(workflow_id, with_params)| {
            StepAction::RunWorkflow {
                workflow_id,
                params: if with_params {
                    Some(serde_json::json!({"p": 1}))
                } else {
                    None
                },
            }
        }),
        (0i64..100_000).prop_map(|event_id| StepAction::MarkEventHandled { event_id }),
        arb_name().prop_map(|approval_code| StepAction::ValidateApproval { approval_code }),
        (arb_name(), arb_name()).prop_map(|(action_type, _)| StepAction::Custom {
            action_type,
            payload: serde_json::json!({}),
        }),
    ]
}

/// Generate an arbitrary Precondition (excluding StepCompleted for simplicity).
fn arb_precondition() -> impl Strategy<Value = Precondition> {
    prop_oneof![
        arb_pane_id().prop_map(|pane_id| Precondition::PaneExists { pane_id }),
        (
            arb_pane_id(),
            prop::option::of(arb_name()),
            prop::option::of(arb_name())
        )
            .prop_map(|(pane_id, expected_agent, expected_domain)| {
                Precondition::PaneState {
                    pane_id,
                    expected_agent,
                    expected_domain,
                }
            }),
        (
            arb_name(),
            prop::option::of(arb_pane_id()),
            prop::option::of(arb_timeout())
        )
            .prop_map(
                |(rule_id, pane_id, within_ms)| Precondition::PatternMatched {
                    rule_id,
                    pane_id,
                    within_ms,
                }
            ),
        (arb_name(), prop::option::of(arb_pane_id()))
            .prop_map(|(rule_id, pane_id)| Precondition::PatternNotMatched { rule_id, pane_id }),
        arb_name().prop_map(|lock_name| Precondition::LockHeld { lock_name }),
        arb_name().prop_map(|lock_name| Precondition::LockAvailable { lock_name }),
        (arb_name(), arb_name(), prop::option::of(arb_pane_id())).prop_map(
            |(workspace_id, action_kind, pane_id)| Precondition::ApprovalValid {
                scope: ApprovalScopeRef {
                    workspace_id,
                    action_kind,
                    pane_id,
                },
            }
        ),
        (arb_name(), arb_name())
            .prop_map(|(name, expression)| Precondition::Custom { name, expression }),
    ]
}

/// Generate an arbitrary VerificationStrategy.
fn arb_verification_strategy() -> impl Strategy<Value = VerificationStrategy> {
    prop_oneof![
        (arb_name(), prop::option::of(arb_pane_id()))
            .prop_map(|(rule_id, pane_id)| VerificationStrategy::PatternMatch { rule_id, pane_id }),
        (prop::option::of(arb_pane_id()), arb_timeout()).prop_map(
            |(pane_id, idle_threshold_ms)| VerificationStrategy::PaneIdle {
                pane_id,
                idle_threshold_ms,
            }
        ),
        (arb_name(), prop::option::of(arb_pane_id()), arb_timeout()).prop_map(
            |(rule_id, pane_id, wait_ms)| VerificationStrategy::PatternAbsent {
                rule_id,
                pane_id,
                wait_ms,
            }
        ),
        (arb_name(), arb_name())
            .prop_map(|(name, expression)| VerificationStrategy::Custom { name, expression }),
        Just(VerificationStrategy::None),
    ]
}

/// Generate an arbitrary Verification.
fn arb_verification() -> impl Strategy<Value = Verification> {
    (
        arb_verification_strategy(),
        prop::option::of(arb_name()),
        prop::option::of(arb_timeout()),
    )
        .prop_map(|(strategy, description, timeout_ms)| Verification {
            strategy,
            description,
            timeout_ms,
        })
}

/// Generate a float that roundtrips cleanly through JSON serialization.
/// Uses integer-based tenths to avoid floating-point representation issues.
fn arb_clean_f64() -> impl Strategy<Value = f64> {
    (10u32..50).prop_map(|n| n as f64 / 10.0)
}

/// Generate an arbitrary OnFailure (excluding Fallback to avoid nested StepPlan).
fn arb_on_failure() -> impl Strategy<Value = OnFailure> {
    prop_oneof![
        prop::option::of(arb_name()).prop_map(|message| OnFailure::Abort { message }),
        (
            1u32..10,
            arb_timeout(),
            prop::option::of(arb_timeout()),
            prop::option::of(arb_clean_f64())
        )
            .prop_map(
                |(max_attempts, initial_delay_ms, max_delay_ms, backoff_multiplier)| {
                    OnFailure::Retry {
                        max_attempts,
                        initial_delay_ms,
                        max_delay_ms,
                        backoff_multiplier,
                    }
                }
            ),
        prop::option::of(any::<bool>()).prop_map(|warn| OnFailure::Skip { warn }),
        arb_name().prop_map(|summary| OnFailure::RequireApproval { summary }),
    ]
}

/// Generate an arbitrary StepPlan for a given step number.
fn arb_step_plan_numbered(step_number: u32) -> impl Strategy<Value = StepPlan> {
    (arb_step_action(), arb_name())
        .prop_map(move |(action, description)| StepPlan::new(step_number, action, description))
}

// =============================================================================
// 1. canonical_string() idempotency for WaitCondition
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn wait_condition_canonical_idempotent(cond in arb_wait_condition()) {
        let s1 = cond.canonical_string();
        let s2 = cond.canonical_string();
        prop_assert_eq!(&s1, &s2, "WaitCondition canonical_string not idempotent");
        prop_assert!(!s1.is_empty(), "canonical_string should not be empty");
    }
}

// =============================================================================
// 2. canonical_string() idempotency for StepAction
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn step_action_canonical_idempotent(action in arb_step_action()) {
        let s1 = action.canonical_string();
        let s2 = action.canonical_string();
        prop_assert_eq!(&s1, &s2, "StepAction canonical_string not idempotent");
        prop_assert!(!s1.is_empty(), "canonical_string should not be empty");
    }
}

// =============================================================================
// 3. canonical_string() idempotency for Precondition
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn precondition_canonical_idempotent(precond in arb_precondition()) {
        let s1 = precond.canonical_string();
        let s2 = precond.canonical_string();
        prop_assert_eq!(&s1, &s2, "Precondition canonical_string not idempotent");
        prop_assert!(!s1.is_empty(), "canonical_string should not be empty");
    }
}

// =============================================================================
// 4. canonical_string() idempotency for VerificationStrategy
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn verification_strategy_canonical_idempotent(vs in arb_verification_strategy()) {
        let s1 = vs.canonical_string();
        let s2 = vs.canonical_string();
        prop_assert_eq!(&s1, &s2, "VerificationStrategy canonical_string not idempotent");
    }
}

// =============================================================================
// 5. canonical_string() idempotency for Verification
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn verification_canonical_idempotent(v in arb_verification()) {
        let s1 = v.canonical_string();
        let s2 = v.canonical_string();
        prop_assert_eq!(&s1, &s2, "Verification canonical_string not idempotent");
    }
}

// =============================================================================
// 6. canonical_string() idempotency for OnFailure
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn on_failure_canonical_idempotent(of in arb_on_failure()) {
        let s1 = of.canonical_string();
        let s2 = of.canonical_string();
        prop_assert_eq!(&s1, &s2, "OnFailure canonical_string not idempotent");
        prop_assert!(!s1.is_empty(), "canonical_string should not be empty");
    }
}

// =============================================================================
// 7. canonical_string() idempotency for StepPlan
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn step_plan_canonical_idempotent(step in arb_step_plan_numbered(1)) {
        let s1 = step.canonical_string();
        let s2 = step.canonical_string();
        prop_assert_eq!(&s1, &s2, "StepPlan canonical_string not idempotent");
    }
}

// =============================================================================
// 8. compute_hash() determinism: identical plans produce identical hashes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn compute_hash_deterministic(
        title in arb_name(),
        ws in arb_name(),
        action in arb_step_action(),
        desc in arb_name(),
    ) {
        let plan1 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(1, action.clone(), &desc))
            .build();
        // Rebuild from scratch with same inputs
        let plan2 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(1, action, &desc))
            .build();
        prop_assert_eq!(plan1.compute_hash(), plan2.compute_hash(),
            "Same content should produce identical hashes");
    }
}

// =============================================================================
// 9. compute_hash() excludes created_at
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hash_excludes_created_at(
        title in arb_name(),
        ws in arb_name(),
        ts1 in 1000i64..999_999,
        ts2 in 1_000_000i64..9_999_999,
    ) {
        let plan1 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .created_at(ts1)
            .build();
        let plan2 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .created_at(ts2)
            .build();
        prop_assert_eq!(plan1.compute_hash(), plan2.compute_hash(),
            "created_at should not affect hash");
    }
}

// =============================================================================
// 10. compute_hash() excludes metadata
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hash_excludes_metadata(
        title in arb_name(),
        ws in arb_name(),
        key in arb_name(),
    ) {
        let plan1 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .metadata(serde_json::json!({"key": key}))
            .build();
        let plan2 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .metadata(serde_json::json!({"different_key": "different_value"}))
            .build();
        prop_assert_eq!(plan1.compute_hash(), plan2.compute_hash(),
            "metadata should not affect hash");
    }
}

// =============================================================================
// 11. compute_hash() includes workspace_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hash_includes_workspace(
        title in arb_name(),
        ws1 in "ws-[a-z]{4}",
        ws2 in "ws-[a-z]{4}",
    ) {
        prop_assume!(ws1 != ws2);
        let plan1 = ActionPlan::builder(&title, &ws1)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .build();
        let plan2 = ActionPlan::builder(&title, &ws2)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .build();
        prop_assert_ne!(plan1.compute_hash(), plan2.compute_hash(),
            "Different workspace_id should produce different hashes");
    }
}

// =============================================================================
// 12. compute_hash() includes title
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hash_includes_title(
        title1 in "title-[a-z]{4}",
        title2 in "title-[a-z]{4}",
        ws in arb_name(),
    ) {
        prop_assume!(title1 != title2);
        let plan1 = ActionPlan::builder(&title1, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .build();
        let plan2 = ActionPlan::builder(&title2, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .build();
        prop_assert_ne!(plan1.compute_hash(), plan2.compute_hash(),
            "Different titles should produce different hashes");
    }
}

// =============================================================================
// 13. compute_hash() includes steps
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hash_includes_steps(
        title in arb_name(),
        ws in arb_name(),
        pane1 in arb_pane_id(),
        pane2 in arb_pane_id(),
    ) {
        prop_assume!(pane1 != pane2);
        let plan1 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: pane1, text: "cmd".into(), paste_mode: None },
                "step",
            ))
            .build();
        let plan2 = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: pane2, text: "cmd".into(), paste_mode: None },
                "step",
            ))
            .build();
        prop_assert_ne!(plan1.compute_hash(), plan2.compute_hash(),
            "Different step content should produce different hashes");
    }
}

// =============================================================================
// 14. Serde roundtrip for StepAction
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn serde_roundtrip_step_action(action in arb_step_action()) {
        let json = serde_json::to_string(&action).unwrap();
        let parsed: StepAction = serde_json::from_str(&json).unwrap();
        // Compare via canonical_string since StepAction doesn't derive PartialEq
        prop_assert_eq!(
            action.canonical_string(),
            parsed.canonical_string(),
            "Serde roundtrip should preserve canonical form"
        );
    }
}

// =============================================================================
// 15. Serde roundtrip for WaitCondition
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn serde_roundtrip_wait_condition(cond in arb_wait_condition()) {
        let json = serde_json::to_string(&cond).unwrap();
        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            cond.canonical_string(),
            parsed.canonical_string(),
            "Serde roundtrip should preserve WaitCondition"
        );
    }
}

// =============================================================================
// 16. Serde roundtrip for Precondition
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn serde_roundtrip_precondition(precond in arb_precondition()) {
        let json = serde_json::to_string(&precond).unwrap();
        let parsed: Precondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            precond.canonical_string(),
            parsed.canonical_string(),
            "Serde roundtrip should preserve Precondition"
        );
    }
}

// =============================================================================
// 17. Serde roundtrip for VerificationStrategy
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn serde_roundtrip_verification_strategy(vs in arb_verification_strategy()) {
        let json = serde_json::to_string(&vs).unwrap();
        let parsed: VerificationStrategy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            vs.canonical_string(),
            parsed.canonical_string(),
            "Serde roundtrip should preserve VerificationStrategy"
        );
    }
}

// =============================================================================
// 18. Serde roundtrip for OnFailure
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn serde_roundtrip_on_failure(of in arb_on_failure()) {
        let json = serde_json::to_string(&of).unwrap();
        let parsed: OnFailure = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            of.canonical_string(),
            parsed.canonical_string(),
            "Serde roundtrip should preserve OnFailure"
        );
    }
}

// =============================================================================
// 19. Serde roundtrip for full ActionPlan
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn serde_roundtrip_action_plan(
        title in arb_name(),
        ws in arb_name(),
        action in arb_step_action(),
        desc in arb_name(),
    ) {
        let plan = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(1, action, &desc))
            .build();

        let json = serde_json::to_string(&plan).unwrap();
        let parsed: ActionPlan = serde_json::from_str(&json).unwrap();

        let hash_before = plan.compute_hash();
        let hash_after = parsed.compute_hash();
        prop_assert_eq!(plan.plan_id, parsed.plan_id, "plan_id should survive roundtrip");
        prop_assert_eq!(&plan.title, &parsed.title, "title should survive roundtrip");
        prop_assert_eq!(&plan.workspace_id, &parsed.workspace_id, "workspace_id should survive roundtrip");
        prop_assert_eq!(hash_before, hash_after,
            "hash should be identical after roundtrip");
    }
}

// =============================================================================
// 20. PlanId::from_hash strips sha256: prefix
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn plan_id_from_hash_strips_prefix(hash in "[a-f0-9]{32}") {
        let with_prefix = format!("sha256:{}", hash);
        let id_with = PlanId::from_hash(&with_prefix);
        let id_without = PlanId::from_hash(&hash);
        let starts = id_with.0.starts_with("plan:");
        prop_assert!(starts, "PlanId should start with plan:");
        prop_assert_eq!(id_with, id_without,
            "from_hash should strip sha256: prefix");
    }
}

// =============================================================================
// 21. PlanId::placeholder recognized by is_placeholder
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn plan_id_non_placeholder_from_hash(hash in "[a-f0-9]{16,64}") {
        let id = PlanId::from_hash(&hash);
        let is_ph = id.is_placeholder();
        prop_assert!(!is_ph, "from_hash IDs should not be placeholder");
    }
}

// =============================================================================
// 22. PlanId Display
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn plan_id_display(hash in "[a-f0-9]{16}") {
        let id = PlanId::from_hash(&hash);
        let display = format!("{}", id);
        let expected = format!("plan:{}", hash);
        prop_assert_eq!(display, expected, "Display should match inner string");
    }
}

// =============================================================================
// 23. IdempotencyKey::for_action determinism
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn idempotency_key_deterministic(
        ws in arb_name(),
        step_num in arb_step_number(),
        action in arb_step_action(),
    ) {
        let key1 = IdempotencyKey::for_action(&ws, step_num, &action);
        let key2 = IdempotencyKey::for_action(&ws, step_num, &action);
        prop_assert_eq!(key1, key2, "for_action should be deterministic");
    }
}

// =============================================================================
// 24. IdempotencyKey::for_action differs by workspace
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn idempotency_key_differs_by_workspace(
        ws1 in "ws-[a-z]{4}",
        ws2 in "ws-[a-z]{4}",
    ) {
        prop_assume!(ws1 != ws2);
        let action = StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None };
        let key1 = IdempotencyKey::for_action(&ws1, 1, &action);
        let key2 = IdempotencyKey::for_action(&ws2, 1, &action);
        prop_assert_ne!(key1, key2, "Different workspace should produce different keys");
    }
}

// =============================================================================
// 25. IdempotencyKey::for_action differs by step number
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn idempotency_key_differs_by_step_number(
        step1 in 1u32..50,
        step2 in 51u32..100,
    ) {
        let action = StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None };
        let key1 = IdempotencyKey::for_action("ws", step1, &action);
        let key2 = IdempotencyKey::for_action("ws", step2, &action);
        prop_assert_ne!(key1, key2, "Different step number should produce different keys");
    }
}

// =============================================================================
// 26. IdempotencyKey::for_action differs by action
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn idempotency_key_differs_by_action(
        pane1 in 0u64..500,
        pane2 in 500u64..1000,
    ) {
        let action1 = StepAction::SendText { pane_id: pane1, text: "x".into(), paste_mode: None };
        let action2 = StepAction::SendText { pane_id: pane2, text: "x".into(), paste_mode: None };
        let key1 = IdempotencyKey::for_action("ws", 1, &action1);
        let key2 = IdempotencyKey::for_action("ws", 1, &action2);
        prop_assert_ne!(key1, key2, "Different action should produce different keys");
    }
}

// =============================================================================
// 27. IdempotencyKey Display
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn idempotency_key_display(hash in "[a-f0-9]{16}") {
        let key = IdempotencyKey::from_hash(&hash);
        let display = format!("{}", key);
        let expected = format!("step:{}", hash);
        prop_assert_eq!(display, expected, "Display should match inner string");
    }
}

// =============================================================================
// 28. StepPlan::new sets correct defaults
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn step_plan_new_defaults(
        step_num in arb_step_number(),
        action in arb_step_action(),
        desc in arb_name(),
    ) {
        let step = StepPlan::new(step_num, action, &desc);
        prop_assert_eq!(step.step_number, step_num, "step_number should match");
        prop_assert_eq!(&step.description, &desc, "description should match");
        prop_assert!(step.preconditions.is_empty(), "preconditions should be empty");
        let has_verification = step.verification.is_some();
        prop_assert!(!has_verification, "verification should be None");
        let has_on_failure = step.on_failure.is_some();
        prop_assert!(!has_on_failure, "on_failure should be None");
        let has_timeout = step.timeout_ms.is_some();
        prop_assert!(!has_timeout, "timeout_ms should be None");
        prop_assert!(!step.idempotent, "idempotent should be false");
        // step_id should start with "step:"
        let starts_step = step.step_id.0.starts_with("step:");
        prop_assert!(starts_step, "step_id should start with step:");
    }
}

// =============================================================================
// 29. StepPlan fluent builder methods
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn step_plan_fluent_builders(
        step_num in arb_step_number(),
        action in arb_step_action(),
        desc in arb_name(),
        precond in arb_precondition(),
        verification in arb_verification(),
        on_failure in arb_on_failure(),
        timeout in arb_timeout(),
    ) {
        let step = StepPlan::new(step_num, action, &desc)
            .with_precondition(precond)
            .with_verification(verification)
            .with_on_failure(on_failure)
            .with_timeout_ms(timeout)
            .idempotent();

        prop_assert_eq!(step.preconditions.len(), 1, "should have one precondition");
        let has_v = step.verification.is_some();
        prop_assert!(has_v, "should have verification");
        let has_f = step.on_failure.is_some();
        prop_assert!(has_f, "should have on_failure");
        prop_assert_eq!(step.timeout_ms, Some(timeout), "should have timeout");
        prop_assert!(step.idempotent, "should be idempotent");
    }
}

// =============================================================================
// 30. ActionPlanBuilder sets correct version and non-placeholder ID
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn builder_sets_version_and_hash(
        title in arb_name(),
        ws in arb_name(),
        action in arb_step_action(),
        desc in arb_name(),
    ) {
        let plan = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(1, action, &desc))
            .build();

        prop_assert_eq!(plan.plan_version, PLAN_SCHEMA_VERSION,
            "plan_version should be PLAN_SCHEMA_VERSION");
        let is_ph = plan.plan_id.is_placeholder();
        prop_assert!(!is_ph, "plan_id should not be placeholder after build");
        // plan_id should start with plan:
        let starts = plan.plan_id.0.starts_with("plan:");
        prop_assert!(starts, "plan_id should start with plan:");
    }
}

// =============================================================================
// 31. validate() passes for sequential step numbers
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn validate_passes_sequential_steps(
        count in 1u32..6,
        title in arb_name(),
        ws in arb_name(),
    ) {
        let mut builder = ActionPlan::builder(&title, &ws);
        for i in 1..=count {
            builder = builder.add_step(StepPlan::new(
                i,
                StepAction::SendText { pane_id: 0, text: format!("cmd{}", i), paste_mode: None },
                format!("Step {}", i),
            ));
        }
        let plan = builder.build();
        let result = plan.validate();
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "Sequential step numbers should validate OK");
    }
}

// =============================================================================
// 32. validate() fails for non-sequential step numbers
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn validate_fails_non_sequential_steps(
        title in arb_name(),
        ws in arb_name(),
        bad_num in 3u32..100,
    ) {
        let mut plan = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "a".into(), paste_mode: None },
                "step 1",
            ))
            .build();
        // Overwrite step_number to be wrong
        plan.steps[0].step_number = bad_num;
        let result = plan.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "Non-sequential step numbers should fail validation");
        let is_invalid = matches!(result, Err(PlanValidationError::InvalidStepNumber { .. }));
        prop_assert!(is_invalid, "Error should be InvalidStepNumber");
    }
}

// =============================================================================
// 33. validate() detects duplicate step IDs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn validate_detects_duplicate_step_ids(
        title in arb_name(),
        ws in arb_name(),
    ) {
        let mut plan = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "a".into(), paste_mode: None },
                "step 1",
            ))
            .add_step(StepPlan::new(
                2,
                StepAction::SendText { pane_id: 1, text: "b".into(), paste_mode: None },
                "step 2",
            ))
            .build();
        // Force duplicate step_id
        plan.steps[1].step_id = plan.steps[0].step_id.clone();
        let result = plan.validate();
        let is_dup = matches!(result, Err(PlanValidationError::DuplicateStepId(_)));
        prop_assert!(is_dup, "Should detect duplicate step IDs");
    }
}

// =============================================================================
// 34. validate() detects unknown step references in preconditions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn validate_detects_unknown_step_reference(
        title in arb_name(),
        ws in arb_name(),
        fake_hash in "[a-f0-9]{16}",
    ) {
        let mut plan = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "a".into(), paste_mode: None },
                "step 1",
            ))
            .build();
        plan.preconditions.push(Precondition::StepCompleted {
            step_id: IdempotencyKey::from_hash(&fake_hash),
        });
        let result = plan.validate();
        let is_unknown = matches!(result, Err(PlanValidationError::UnknownStepReference(_)));
        prop_assert!(is_unknown, "Should detect unknown step reference");
    }
}

// =============================================================================
// 35. action_type_name() returns correct string for each variant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn action_type_name_correct(action in arb_step_action()) {
        let name = action.action_type_name();
        // Name should be one of the known types
        let valid_names = [
            "send_text", "wait_for", "acquire_lock", "release_lock",
            "store_data", "run_workflow", "mark_event_handled",
            "validate_approval", "nested_plan", "custom",
        ];
        let is_valid = valid_names.contains(&name);
        prop_assert!(is_valid, "action_type_name should be a valid name, got: {}", name);
        // Should be non-empty
        prop_assert!(!name.is_empty(), "action_type_name should not be empty");
    }
}

// =============================================================================
// 36. OnFailure::abort() factory
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn on_failure_abort_factory(_dummy in 0u8..1) {
        let f = OnFailure::abort();
        let is_abort = matches!(f, OnFailure::Abort { message: None });
        prop_assert!(is_abort, "abort() should produce Abort with no message");
    }
}

// =============================================================================
// 37. OnFailure::abort_with_message() factory
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn on_failure_abort_with_message_factory(msg in arb_name()) {
        let f = OnFailure::abort_with_message(&msg);
        let is_abort_msg = matches!(&f, OnFailure::Abort { message: Some(m) } if m == &msg);
        prop_assert!(is_abort_msg, "abort_with_message should produce Abort with message");
    }
}

// =============================================================================
// 38. OnFailure::retry() factory
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn on_failure_retry_factory(
        max_att in 1u32..10,
        delay in arb_timeout(),
    ) {
        let f = OnFailure::retry(max_att, delay);
        let is_retry = matches!(
            &f,
            OnFailure::Retry {
                max_attempts,
                initial_delay_ms,
                max_delay_ms: None,
                backoff_multiplier: None,
            } if *max_attempts == max_att && *initial_delay_ms == delay
        );
        prop_assert!(is_retry, "retry() should produce Retry with correct fields");
    }
}

// =============================================================================
// 39. OnFailure::skip() factory
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn on_failure_skip_factory(_dummy in 0u8..1) {
        let f = OnFailure::skip();
        let is_skip = matches!(f, OnFailure::Skip { warn: Some(true) });
        prop_assert!(is_skip, "skip() should produce Skip with warn=true");
    }
}

// =============================================================================
// 40. Verification::pattern_match() factory
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn verification_pattern_match_factory(rule_id in arb_name()) {
        let v = Verification::pattern_match(&rule_id);
        let is_pm = matches!(
            &v.strategy,
            VerificationStrategy::PatternMatch { rule_id: r, pane_id: None } if r == &rule_id
        );
        prop_assert!(is_pm, "pattern_match should produce PatternMatch strategy");
        let no_desc = v.description.is_none();
        prop_assert!(no_desc, "description should be None");
        let no_timeout = v.timeout_ms.is_none();
        prop_assert!(no_timeout, "timeout_ms should be None");
    }
}

// =============================================================================
// 41. Verification::pane_idle() factory
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn verification_pane_idle_factory(threshold in arb_timeout()) {
        let v = Verification::pane_idle(threshold);
        let is_pi = matches!(
            &v.strategy,
            VerificationStrategy::PaneIdle { pane_id: None, idle_threshold_ms } if *idle_threshold_ms == threshold
        );
        prop_assert!(is_pi, "pane_idle should produce PaneIdle strategy");
    }
}

// =============================================================================
// 42. Verification with_description and with_timeout_ms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn verification_fluent_builders(
        rule_id in arb_name(),
        desc in arb_name(),
        timeout in arb_timeout(),
    ) {
        let v = Verification::pattern_match(&rule_id)
            .with_description(&desc)
            .with_timeout_ms(timeout);
        let has_desc = v.description.as_deref() == Some(desc.as_str());
        prop_assert!(has_desc, "with_description should set description");
        prop_assert_eq!(v.timeout_ms, Some(timeout), "with_timeout_ms should set timeout");
    }
}

// =============================================================================
// 43. step_count() and has_preconditions()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn step_count_and_has_preconditions(
        count in 1u32..5,
        title in arb_name(),
        ws in arb_name(),
        add_precond in any::<bool>(),
    ) {
        let mut builder = ActionPlan::builder(&title, &ws);
        for i in 1..=count {
            builder = builder.add_step(StepPlan::new(
                i,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                format!("step {}", i),
            ));
        }
        if add_precond {
            builder = builder.add_precondition(Precondition::PaneExists { pane_id: 0 });
        }
        let plan = builder.build();
        prop_assert_eq!(plan.step_count(), count as usize, "step_count should match");
        prop_assert_eq!(plan.has_preconditions(), add_precond,
            "has_preconditions should reflect whether preconditions were added");
    }
}

// =============================================================================
// 44. compute_hash() format: starts with sha256: and has correct length
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn hash_format(
        title in arb_name(),
        ws in arb_name(),
    ) {
        let plan = ActionPlan::builder(&title, &ws)
            .add_step(StepPlan::new(
                1,
                StepAction::SendText { pane_id: 0, text: "x".into(), paste_mode: None },
                "step",
            ))
            .build();
        let hash = plan.compute_hash();
        let starts_sha = hash.starts_with("sha256:");
        prop_assert!(starts_sha, "hash should start with sha256:");
        // sha256: (7 chars) + 32 hex chars = 39
        prop_assert_eq!(hash.len(), 39, "hash should be 39 chars (sha256: + 32 hex)");
    }
}

// =============================================================================
// 45. Serde roundtrip for Verification
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn serde_roundtrip_verification(v in arb_verification()) {
        let json = serde_json::to_string(&v).unwrap();
        let parsed: Verification = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(
            v.canonical_string(),
            parsed.canonical_string(),
            "Serde roundtrip should preserve Verification"
        );
    }
}
