// Property-based tests for workflows/step_results module.
//
// Covers: serde roundtrips for StepResult, TextMatch, WaitCondition,
// plus behavioral invariants for constructors and predicates.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::workflows::{StepResult, TextMatch, WaitCondition};

// =============================================================================
// Strategies
// =============================================================================

fn arb_text_match() -> impl Strategy<Value = TextMatch> {
    prop_oneof![
        "[a-z ]{3,20}".prop_map(|value| TextMatch::Substring { value }),
        "[a-z]+".prop_map(|pattern| TextMatch::Regex { pattern }),
    ]
}

fn arb_wait_condition() -> impl Strategy<Value = WaitCondition> {
    prop_oneof![
        (prop::option::of(any::<u64>()), "[a-z_]{3,15}")
            .prop_map(|(pane_id, rule_id)| { WaitCondition::Pattern { pane_id, rule_id } }),
        (prop::option::of(any::<u64>()), 100u64..120_000).prop_map(
            |(pane_id, idle_threshold_ms)| {
                WaitCondition::PaneIdle {
                    pane_id,
                    idle_threshold_ms,
                }
            }
        ),
        (prop::option::of(any::<u64>()), 100u64..120_000).prop_map(|(pane_id, stable_for_ms)| {
            WaitCondition::StableTail {
                pane_id,
                stable_for_ms,
            }
        }),
        (prop::option::of(any::<u64>()), arb_text_match())
            .prop_map(|(pane_id, matcher)| { WaitCondition::TextMatch { pane_id, matcher } }),
        (100u64..120_000).prop_map(|duration_ms| WaitCondition::Sleep { duration_ms }),
        "[a-z_]{3,15}".prop_map(|key| WaitCondition::External { key }),
    ]
}

fn arb_step_result() -> impl Strategy<Value = StepResult> {
    prop_oneof![
        Just(StepResult::Continue),
        // Use simple JSON values to avoid roundtrip precision issues
        prop_oneof![
            Just(serde_json::Value::Null),
            any::<i64>().prop_map(|n| serde_json::json!(n)),
            "[a-z ]{1,20}".prop_map(|s| serde_json::json!(s)),
            any::<bool>().prop_map(|b| serde_json::json!(b)),
        ]
        .prop_map(|result| StepResult::Done { result }),
        (100u64..120_000).prop_map(|delay_ms| StepResult::Retry { delay_ms }),
        "[a-z ]{5,30}".prop_map(|reason| StepResult::Abort { reason }),
        (arb_wait_condition(), prop::option::of(1000u64..120_000)).prop_map(
            |(condition, timeout_ms)| StepResult::WaitFor {
                condition,
                timeout_ms,
            }
        ),
        (
            "[a-z ]{3,30}",
            prop::option::of(arb_wait_condition()),
            prop::option::of(1000u64..120_000),
        )
            .prop_map(|(text, wait_for, wait_timeout_ms)| StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            }),
        (0usize..100).prop_map(|step| StepResult::JumpTo { step }),
    ]
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn text_match_serde_roundtrip(val in arb_text_match()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TextMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &back);
    }

    #[test]
    fn wait_condition_serde_roundtrip(val in arb_wait_condition()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val, &back);
    }

    #[test]
    fn step_result_serde_roundtrip(val in arb_step_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: StepResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }
}

// =============================================================================
// Tag / discriminator tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn text_match_serializes_with_kind_tag(val in arb_text_match()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"kind\":"));
        match &val {
            TextMatch::Substring { .. } => prop_assert!(json.contains("\"kind\":\"substring\"")),
            TextMatch::Regex { .. } => prop_assert!(json.contains("\"kind\":\"regex\"")),
        }
    }

    #[test]
    fn wait_condition_serializes_with_type_tag(val in arb_wait_condition()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"type\":"));
        match &val {
            WaitCondition::Pattern { .. } => prop_assert!(json.contains("\"type\":\"pattern\"")),
            WaitCondition::PaneIdle { .. } => prop_assert!(json.contains("\"type\":\"pane_idle\"")),
            WaitCondition::StableTail { .. } => prop_assert!(json.contains("\"type\":\"stable_tail\"")),
            WaitCondition::TextMatch { .. } => prop_assert!(json.contains("\"type\":\"text_match\"")),
            WaitCondition::Sleep { .. } => prop_assert!(json.contains("\"type\":\"sleep\"")),
            WaitCondition::External { .. } => prop_assert!(json.contains("\"type\":\"external\"")),
        }
    }

    #[test]
    fn step_result_serializes_with_type_tag(val in arb_step_result()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"type\":"));
        match &val {
            StepResult::Continue => prop_assert!(json.contains("\"type\":\"continue\"")),
            StepResult::Done { .. } => prop_assert!(json.contains("\"type\":\"done\"")),
            StepResult::Retry { .. } => prop_assert!(json.contains("\"type\":\"retry\"")),
            StepResult::Abort { .. } => prop_assert!(json.contains("\"type\":\"abort\"")),
            StepResult::WaitFor { .. } => prop_assert!(json.contains("\"type\":\"wait_for\"")),
            StepResult::SendText { .. } => prop_assert!(json.contains("\"type\":\"send_text\"")),
            StepResult::JumpTo { .. } => prop_assert!(json.contains("\"type\":\"jump_to\"")),
        }
    }
}

// =============================================================================
// Behavioral invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn step_result_continue_predicates(_dummy in 0u8..1) {
        let r = StepResult::cont();
        prop_assert!(r.is_continue());
        prop_assert!(!r.is_done());
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_send_text());
    }

    #[test]
    fn step_result_done_is_terminal(result in prop_oneof![
        Just(serde_json::Value::Null),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
    ]) {
        let r = StepResult::done(result);
        prop_assert!(r.is_done());
        prop_assert!(r.is_terminal());
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_send_text());
    }

    #[test]
    fn step_result_done_empty_is_null(_dummy in 0u8..1) {
        let r = StepResult::done_empty();
        prop_assert!(r.is_done());
        if let StepResult::Done { result } = &r {
            prop_assert!(result.is_null());
        } else {
            prop_assert!(false, "Expected Done variant");
        }
    }

    #[test]
    fn step_result_abort_is_terminal(reason in "[a-z ]{5,30}") {
        let r = StepResult::abort(&reason);
        prop_assert!(r.is_terminal());
        prop_assert!(!r.is_done());
        prop_assert!(!r.is_continue());
        if let StepResult::Abort { reason: r_reason } = &r {
            prop_assert_eq!(&reason, r_reason);
        }
    }

    #[test]
    fn step_result_retry_not_terminal(delay_ms in 100u64..120_000) {
        let r = StepResult::retry(delay_ms);
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_done());
        prop_assert!(!r.is_continue());
        if let StepResult::Retry { delay_ms: d } = r {
            prop_assert_eq!(delay_ms, d);
        }
    }

    #[test]
    fn step_result_send_text_predicate(text in "[a-z ]{3,30}") {
        let r = StepResult::send_text(&text);
        prop_assert!(r.is_send_text());
        prop_assert!(!r.is_terminal());
        if let StepResult::SendText { text: t, wait_for, wait_timeout_ms } = &r {
            prop_assert_eq!(&text, t);
            prop_assert!(wait_for.is_none());
            prop_assert!(wait_timeout_ms.is_none());
        }
    }

    #[test]
    fn step_result_send_text_and_wait(
        text in "[a-z ]{3,30}",
        timeout_ms in 1000u64..120_000
    ) {
        let matcher = TextMatch::substring("done");
        let cond = WaitCondition::text_match(matcher);
        let r = StepResult::send_text_and_wait(&text, cond, timeout_ms);
        prop_assert!(r.is_send_text());
        if let StepResult::SendText { wait_for, wait_timeout_ms, .. } = &r {
            prop_assert!(wait_for.is_some());
            prop_assert_eq!(*wait_timeout_ms, Some(timeout_ms));
        }
    }

    #[test]
    fn step_result_jump_to_value(step in 0usize..1000) {
        let r = StepResult::jump_to(step);
        if let StepResult::JumpTo { step: s } = r {
            prop_assert_eq!(step, s);
        } else {
            prop_assert!(false, "Expected JumpTo variant");
        }
    }

    #[test]
    fn wait_condition_pane_id_none_for_default_constructors(
        rule_id in "[a-z_]{3,15}",
        idle_ms in 100u64..120_000,
        stable_ms in 100u64..120_000,
        sleep_ms in 100u64..120_000,
        key in "[a-z_]{3,15}",
    ) {
        prop_assert!(WaitCondition::pattern(&rule_id).pane_id().is_none());
        prop_assert!(WaitCondition::pane_idle(idle_ms).pane_id().is_none());
        prop_assert!(WaitCondition::stable_tail(stable_ms).pane_id().is_none());
        prop_assert!(WaitCondition::text_match(TextMatch::substring("test")).pane_id().is_none());
        prop_assert!(WaitCondition::sleep(sleep_ms).pane_id().is_none());
        prop_assert!(WaitCondition::external(&key).pane_id().is_none());
    }

    #[test]
    fn wait_condition_pane_id_some_for_on_pane_constructors(
        pane_id in any::<u64>(),
        rule_id in "[a-z_]{3,15}",
        idle_ms in 100u64..120_000,
        stable_ms in 100u64..120_000,
    ) {
        prop_assert_eq!(WaitCondition::pattern_on_pane(pane_id, &rule_id).pane_id(), Some(pane_id));
        prop_assert_eq!(WaitCondition::pane_idle_on(pane_id, idle_ms).pane_id(), Some(pane_id));
        prop_assert_eq!(WaitCondition::stable_tail_on(pane_id, stable_ms).pane_id(), Some(pane_id));
        prop_assert_eq!(
            WaitCondition::text_match_on_pane(pane_id, TextMatch::substring("x")).pane_id(),
            Some(pane_id)
        );
    }

    #[test]
    fn wait_condition_sleep_and_external_never_have_pane_id(val in arb_wait_condition()) {
        match &val {
            WaitCondition::Sleep { .. } | WaitCondition::External { .. } => {
                prop_assert!(val.pane_id().is_none());
            }
            _ => { /* other variants can have pane_id */ }
        }
    }

    #[test]
    fn text_match_constructors_match_variant(val in arb_text_match()) {
        let debug = format!("{val:?}");
        prop_assert!(!debug.is_empty());
        match &val {
            TextMatch::Substring { value } => {
                let constructed = TextMatch::substring(value);
                prop_assert_eq!(&val, &constructed);
            }
            TextMatch::Regex { pattern } => {
                let constructed = TextMatch::regex(pattern);
                prop_assert_eq!(&val, &constructed);
            }
        }
    }

    #[test]
    fn step_result_wait_for_constructor_preserves_condition(cond in arb_wait_condition()) {
        let r = StepResult::wait_for(cond.clone());
        if let StepResult::WaitFor { condition, timeout_ms } = &r {
            prop_assert_eq!(&cond, condition);
            prop_assert!(timeout_ms.is_none());
        } else {
            prop_assert!(false, "Expected WaitFor variant");
        }
    }

    #[test]
    fn step_result_wait_for_with_timeout_preserves_both(
        cond in arb_wait_condition(),
        timeout in 1000u64..120_000
    ) {
        let r = StepResult::wait_for_with_timeout(cond.clone(), timeout);
        if let StepResult::WaitFor { condition, timeout_ms } = &r {
            prop_assert_eq!(&cond, condition);
            prop_assert_eq!(*timeout_ms, Some(timeout));
        } else {
            prop_assert!(false, "Expected WaitFor variant");
        }
    }
}
