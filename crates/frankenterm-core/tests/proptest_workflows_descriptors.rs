// Property-based tests for workflows/descriptors module.
//
// Covers: serde roundtrips for all public Serialize/Deserialize types,
// descriptor validation invariants, failure handler interpolation,
// and step identity/description properties.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::workflows::{
    DescriptorControlKey, DescriptorFailureHandler, DescriptorMatcher, DescriptorStep,
    DescriptorTrigger, WorkflowDescriptor,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_descriptor_trigger() -> impl Strategy<Value = DescriptorTrigger> {
    (
        prop::collection::vec("[a-z_.]{3,15}", 0..4),
        prop::collection::vec(
            prop_oneof![
                Just("codex".to_string()),
                Just("claude_code".to_string()),
                Just("gemini".to_string()),
            ],
            0..3,
        ),
        prop::collection::vec("[a-z_.]{3,15}", 0..4),
    )
        .prop_map(|(event_types, agent_types, rule_ids)| DescriptorTrigger {
            event_types,
            agent_types,
            rule_ids,
        })
        .prop_filter(
            "descriptor trigger must constrain at least one dimension",
            |trigger| {
                !trigger.event_types.is_empty()
                    || !trigger.agent_types.is_empty()
                    || !trigger.rule_ids.is_empty()
            },
        )
}

fn arb_descriptor_failure_handler() -> impl Strategy<Value = DescriptorFailureHandler> {
    prop_oneof![
        "[a-z][a-z ${}_]{4,29}".prop_map(|message| DescriptorFailureHandler::Notify { message }),
        "[a-z][a-z ${}_]{4,29}".prop_map(|message| DescriptorFailureHandler::Log { message }),
        "[a-z][a-z ${}_]{4,29}".prop_map(|message| DescriptorFailureHandler::Abort { message }),
    ]
}

fn arb_descriptor_matcher() -> impl Strategy<Value = DescriptorMatcher> {
    prop_oneof![
        "[a-z][a-z ]{2,19}".prop_map(|value| DescriptorMatcher::Substring { value }),
        // Use safe regex patterns only (no nested quantifiers)
        "[a-z]+".prop_map(|pattern| DescriptorMatcher::Regex { pattern }),
    ]
}

fn arb_descriptor_control_key() -> impl Strategy<Value = DescriptorControlKey> {
    prop_oneof![
        Just(DescriptorControlKey::CtrlC),
        Just(DescriptorControlKey::CtrlD),
        Just(DescriptorControlKey::CtrlZ),
    ]
}

/// Non-recursive descriptor steps (leaves only, no Conditional/Loop).
fn arb_leaf_step() -> impl Strategy<Value = DescriptorStep> {
    prop_oneof![
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            arb_descriptor_matcher(),
            prop::option::of(1000u64..120_000),
        )
            .prop_map(|(id, description, matcher, timeout_ms)| {
                DescriptorStep::WaitFor {
                    id,
                    description,
                    matcher,
                    timeout_ms,
                }
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            100u64..30_000
        )
            .prop_map(|(id, description, duration_ms)| {
                DescriptorStep::Sleep {
                    id,
                    description,
                    duration_ms,
                }
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            "[a-z][a-z ]{2,49}",
            prop_oneof![
                Just((None, None)),
                (arb_descriptor_matcher(), prop::option::of(1000u64..120_000))
                    .prop_map(|(matcher, timeout_ms)| (Some(matcher), timeout_ms)),
            ],
        )
            .prop_map(|(id, description, text, (wait_for, wait_timeout_ms))| {
                DescriptorStep::SendText {
                    id,
                    description,
                    text,
                    wait_for,
                    wait_timeout_ms,
                }
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            arb_descriptor_control_key()
        )
            .prop_map(|(id, description, key)| DescriptorStep::SendCtrl {
                id,
                description,
                key,
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            "[a-z][a-z ]{4,49}"
        )
            .prop_map(|(id, description, message)| DescriptorStep::Notify {
                id,
                description,
                message,
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            "[a-z][a-z ]{4,49}"
        )
            .prop_map(|(id, description, message)| DescriptorStep::Log {
                id,
                description,
                message,
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z][a-z ]{4,19}"),
            "[a-z][a-z ]{4,49}"
        )
            .prop_map(|(id, description, reason)| DescriptorStep::Abort {
                id,
                description,
                reason,
            }),
    ]
}

/// Generates valid WorkflowDescriptor values (schema v1, unique step IDs).
fn arb_workflow_descriptor() -> impl Strategy<Value = WorkflowDescriptor> {
    (
        "[a-z_]{3,20}",
        prop::option::of("[a-z][a-z ]{9,39}"),
        prop::collection::vec(arb_descriptor_trigger(), 0..3),
        // 1..5 leaf steps with unique IDs generated from indices
        (1usize..5).prop_flat_map(|count| {
            prop::collection::vec(arb_leaf_step(), count..=count).prop_map(|mut steps| {
                // Ensure unique IDs by suffixing with index
                for (i, step) in steps.iter_mut().enumerate() {
                    match step {
                        DescriptorStep::WaitFor { id, .. }
                        | DescriptorStep::Sleep { id, .. }
                        | DescriptorStep::SendText { id, .. }
                        | DescriptorStep::SendCtrl { id, .. }
                        | DescriptorStep::Notify { id, .. }
                        | DescriptorStep::Log { id, .. }
                        | DescriptorStep::Abort { id, .. }
                        | DescriptorStep::Conditional { id, .. }
                        | DescriptorStep::Loop { id, .. } => {
                            *id = format!("step_{i}");
                        }
                    }
                }
                steps
            })
        }),
        prop::option::of(arb_descriptor_failure_handler()),
    )
        .prop_map(
            |(name, description, triggers, steps, on_failure)| WorkflowDescriptor {
                workflow_schema_version: 1,
                name,
                description,
                triggers,
                steps,
                on_failure,
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn descriptor_trigger_serde_roundtrip(val in arb_descriptor_trigger()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DescriptorTrigger = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.event_types, back.event_types);
        prop_assert_eq!(val.agent_types, back.agent_types);
        prop_assert_eq!(val.rule_ids, back.rule_ids);
    }

    #[test]
    fn descriptor_failure_handler_serde_roundtrip(val in arb_descriptor_failure_handler()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DescriptorFailureHandler = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn descriptor_matcher_serde_roundtrip(val in arb_descriptor_matcher()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DescriptorMatcher = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn descriptor_control_key_serde_roundtrip(val in arb_descriptor_control_key()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DescriptorControlKey = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn descriptor_step_serde_roundtrip(val in arb_leaf_step()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DescriptorStep = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn workflow_descriptor_serde_roundtrip(val in arb_workflow_descriptor()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowDescriptor = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.workflow_schema_version, back.workflow_schema_version);
        prop_assert_eq!(val.name, back.name);
        prop_assert_eq!(val.description, back.description);
        prop_assert_eq!(val.triggers.len(), back.triggers.len());
        prop_assert_eq!(val.steps.len(), back.steps.len());
    }
}

#[test]
fn descriptor_validation_rejects_over_recursion_limit_before_compile_truncation() {
    let mut step = DescriptorStep::Log {
        id: "leaf".to_string(),
        description: None,
        message: "leaf".to_string(),
    };
    for depth in (0..70).rev() {
        step = DescriptorStep::Loop {
            id: format!("loop_{depth}"),
            description: None,
            count: 1,
            body: vec![step],
        };
    }

    let descriptor = WorkflowDescriptor {
        workflow_schema_version: 1,
        name: "too_deep".to_string(),
        description: None,
        triggers: Vec::new(),
        steps: vec![step],
        on_failure: None,
    };

    let limits = frankenterm_core::workflows::DescriptorLimits::default();
    assert!(
        descriptor.validate(&limits).is_err(),
        "over-deep descriptor trees must fail validation instead of compiling truncated metadata"
    );
}

#[test]
fn descriptor_validation_accepts_nested_unique_step_ids_within_limit() {
    let descriptor = WorkflowDescriptor {
        workflow_schema_version: 1,
        name: "nested_unique".to_string(),
        description: None,
        triggers: Vec::new(),
        steps: vec![DescriptorStep::Conditional {
            id: "branch".to_string(),
            description: None,
            test_text: "yes".to_string(),
            matcher: DescriptorMatcher::Substring {
                value: "yes".to_string(),
            },
            then_steps: vec![DescriptorStep::Loop {
                id: "then_loop".to_string(),
                description: None,
                count: 2,
                body: vec![DescriptorStep::Log {
                    id: "then_log".to_string(),
                    description: None,
                    message: "then".to_string(),
                }],
            }],
            else_steps: vec![DescriptorStep::Notify {
                id: "else_notify".to_string(),
                description: None,
                message: "else".to_string(),
            }],
        }],
        on_failure: None,
    };

    let limits = frankenterm_core::workflows::DescriptorLimits::default();
    assert!(
        descriptor.validate(&limits).is_ok(),
        "nested descriptors with unique ids and valid limits should validate"
    );
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn failure_handler_interpolation_replaces_placeholder(
        handler in arb_descriptor_failure_handler(),
        step_name in "[a-z_]{3,15}"
    ) {
        let result = handler.interpolate_message(&step_name);
        // If the template contained ${failed_step}, it should be replaced
        let template = match &handler {
            DescriptorFailureHandler::Notify { message }
            | DescriptorFailureHandler::Log { message }
            | DescriptorFailureHandler::Abort { message } => message,
        };
        if template.contains("${failed_step}") {
            let has_name = result.contains(&step_name);
            prop_assert!(has_name);
            let no_placeholder = !result.contains("${failed_step}");
            prop_assert!(no_placeholder);
        } else {
            prop_assert_eq!(&result, template);
        }
    }

    #[test]
    fn valid_workflow_descriptor_validates_ok(val in arb_workflow_descriptor()) {
        // Our generated descriptors should always validate (schema v1, unique IDs, within limits)
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        let result = val.validate(&limits);
        let check = result.is_ok();
        prop_assert!(check, "Descriptor validation failed: {:?}", val.name);
    }

    #[test]
    fn wrong_schema_version_fails_validation(
        mut val in arb_workflow_descriptor(),
        bad_version in (2u32..100)
    ) {
        val.workflow_schema_version = bad_version;
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        let check = val.validate(&limits).is_err();
        prop_assert!(check, "Expected validation to fail for schema version {}", bad_version);
    }

    #[test]
    fn empty_workflow_name_fails_validation(mut val in arb_workflow_descriptor()) {
        val.name = " \t ".to_string();
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            val.validate(&limits).is_err(),
            "descriptor validation must reject empty workflow names"
        );
    }

    #[test]
    fn workflow_name_respects_configured_text_length_limit(
        max_text_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".repeat(max_text_len),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "workflow name at max_text_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".repeat(max_text_len.saturating_add(1)),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "workflow name longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn workflow_name_with_whitespace_fails_validation(
        prefix in "[a-z]{1,8}",
        suffix in "[a-z]{1,8}",
    ) {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: format!("{prefix} {suffix}"),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "descriptor validation must reject workflow names containing whitespace"
        );
    }

    #[test]
    fn empty_trigger_values_fail_validation(
        mut val in arb_workflow_descriptor(),
        trigger_field in 0u8..3,
    ) {
        let mut trigger = DescriptorTrigger {
            event_types: vec!["session.compaction".to_string()],
            agent_types: vec!["codex".to_string()],
            rule_ids: vec!["compaction.detected".to_string()],
        };
        match trigger_field {
            0 => trigger.event_types = vec![" \t ".to_string()],
            1 => trigger.agent_types = vec![" \t ".to_string()],
            _ => trigger.rule_ids = vec![" \t ".to_string()],
        }
        val.triggers = vec![trigger];

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            val.validate(&limits).is_err(),
            "descriptor validation must reject empty trigger values"
        );
    }

    #[test]
    fn all_empty_trigger_fails_validation(mut val in arb_workflow_descriptor()) {
        val.triggers = vec![DescriptorTrigger {
            event_types: Vec::new(),
            agent_types: Vec::new(),
            rule_ids: Vec::new(),
        }];
        let limits = frankenterm_core::workflows::DescriptorLimits::default();

        prop_assert!(
            val.validate(&limits).is_err(),
            "descriptor validation must reject match-all trigger entries"
        );
    }

    #[test]
    fn trigger_values_with_whitespace_fail_validation(
        prefix in "[a-z]{1,8}",
        suffix in "[a-z]{1,8}",
        trigger_field in 0u8..3,
    ) {
        let mut trigger = DescriptorTrigger {
            event_types: vec!["session.compaction".to_string()],
            agent_types: vec!["codex".to_string()],
            rule_ids: vec!["compaction.detected".to_string()],
        };
        let whitespace_value = format!("{prefix} {suffix}");
        match trigger_field {
            0 => trigger.event_types = vec![whitespace_value],
            1 => trigger.agent_types = vec![whitespace_value],
            _ => trigger.rule_ids = vec![whitespace_value],
        }

        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "trigger_value_whitespace".to_string(),
            description: None,
            triggers: vec![trigger],
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "descriptor validation must reject trigger values containing whitespace"
        );
    }

    #[test]
    fn trigger_values_with_empty_segments_fail_validation(
        suffix in "[a-z]{1,8}",
        trigger_field in 0u8..3,
    ) {
        let mut trigger = DescriptorTrigger {
            event_types: vec!["session.compaction".to_string()],
            agent_types: vec!["codex".to_string()],
            rule_ids: vec!["compaction.detected".to_string()],
        };
        let segmented_value = format!("bad..{suffix}");
        match trigger_field {
            0 => trigger.event_types = vec![segmented_value],
            1 => trigger.agent_types = vec![segmented_value],
            _ => trigger.rule_ids = vec![segmented_value],
        }

        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "trigger_value_empty_segment".to_string(),
            description: None,
            triggers: vec![trigger],
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "descriptor validation must reject trigger values containing empty dot segments"
        );
    }

    #[test]
    fn unknown_trigger_agent_type_fails_validation(
        mut val in arb_workflow_descriptor(),
        unknown_agent in "[a-z]{4,10}",
    ) {
        prop_assume!(
            !matches!(
                unknown_agent.as_str(),
                "codex" | "claude_code" | "gemini" | "wezterm" | "unknown"
            )
        );
        val.triggers = vec![DescriptorTrigger {
            event_types: vec!["session.compaction".to_string()],
            agent_types: vec![unknown_agent],
            rule_ids: Vec::new(),
        }];
        let limits = frankenterm_core::workflows::DescriptorLimits::default();

        prop_assert!(
            val.validate(&limits).is_err(),
            "descriptor validation must reject unsupported trigger agent_types values"
        );
    }

    #[test]
    fn trigger_values_respect_configured_text_length_limit(
        max_text_len in 5usize..128,
        trigger_field in prop_oneof![Just(0u8), Just(2u8)],
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };
        let build_trigger = |value: String| {
            let mut trigger = DescriptorTrigger {
                event_types: vec!["e".to_string()],
                agent_types: vec!["codex".to_string()],
                rule_ids: vec!["r".to_string()],
            };
            match trigger_field {
                0 => trigger.event_types = vec![value],
                1 => trigger.agent_types = vec![value],
                _ => trigger.rule_ids = vec![value],
            }
            trigger
        };
        let build_descriptor = |trigger: DescriptorTrigger| WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: vec![trigger],
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };

        let at_limit = build_descriptor(build_trigger("x".repeat(max_text_len)));
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "trigger value at max_text_len should validate"
        );

        let over_limit =
            build_descriptor(build_trigger("x".repeat(max_text_len.saturating_add(1))));
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "trigger value longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn step_id_respects_configured_text_length_limit(max_text_len in 1usize..128) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let build_descriptor = |id: String| WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id,
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };

        let at_limit = build_descriptor("x".repeat(max_text_len));
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "step id at max_text_len should validate"
        );

        let over_limit = build_descriptor("x".repeat(max_text_len.saturating_add(1)));
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "step id longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn step_id_with_whitespace_fails_validation(
        prefix in "[a-z]{1,8}",
        suffix in "[a-z]{1,8}",
    ) {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "step_id_whitespace".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: format!("{prefix} {suffix}"),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "descriptor validation must reject step ids containing whitespace"
        );
    }

    #[test]
    fn failure_handler_message_respects_configured_text_length_limit(
        max_text_len in 1usize..128,
        handler_variant in 0u8..3,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };
        let handler = |message: String| match handler_variant {
            0 => DescriptorFailureHandler::Notify { message },
            1 => DescriptorFailureHandler::Log { message },
            _ => DescriptorFailureHandler::Abort { message },
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: Some(handler("x".repeat(max_text_len))),
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "failure handler message at max_text_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: Some(handler("x".repeat(max_text_len.saturating_add(1)))),
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "failure handler message longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn empty_failure_handler_message_fails_validation(handler_variant in 0u8..3) {
        let handler = match handler_variant {
            0 => DescriptorFailureHandler::Notify {
                message: " \t ".to_string(),
            },
            1 => DescriptorFailureHandler::Log {
                message: " \t ".to_string(),
            },
            _ => DescriptorFailureHandler::Abort {
                message: " \t ".to_string(),
            },
        };
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "failure_handler_blank".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: Some(handler),
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "failure handler messages must be non-empty when on_failure is configured"
        );
    }

    #[test]
    fn empty_terminal_step_messages_fail_validation(step_variant in 0u8..3) {
        let step = match step_variant {
            0 => DescriptorStep::Notify {
                id: "s".to_string(),
                description: None,
                message: " \t ".to_string(),
            },
            1 => DescriptorStep::Log {
                id: "s".to_string(),
                description: None,
                message: " \t ".to_string(),
            },
            _ => DescriptorStep::Abort {
                id: "s".to_string(),
                description: None,
                reason: " \t ".to_string(),
            },
        };
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "terminal_step_blank".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![step],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "terminal workflow steps must carry non-empty operator-facing text"
        );
    }

    #[test]
    fn workflow_description_respects_configured_text_length_limit(
        max_text_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: Some("x".repeat(max_text_len)),
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "workflow description at max_text_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: Some("x".repeat(max_text_len.saturating_add(1))),
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "workflow description longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn empty_workflow_description_fails_validation() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "workflow_description_blank".to_string(),
            description: Some(" \t ".to_string()),
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: None,
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "workflow descriptions must be omitted rather than blank"
        );
    }

    #[test]
    fn step_description_respects_configured_text_length_limit(
        max_text_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: Some("x".repeat(max_text_len)),
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "step description at max_text_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: Some("x".repeat(max_text_len.saturating_add(1))),
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "step description longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn empty_step_description_fails_validation() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "step_description_blank".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendCtrl {
                id: "s".to_string(),
                description: Some(" \t ".to_string()),
                key: DescriptorControlKey::CtrlC,
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "step descriptions must be omitted rather than blank"
        );
    }

    #[test]
    fn duplicate_top_level_step_ids_fail_validation(
        name in "[a-z_]{3,20}",
        duplicate_id in "[a-z_]{3,15}",
    ) {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name,
            description: None,
            triggers: Vec::new(),
            steps: vec![
                DescriptorStep::Log {
                    id: duplicate_id.clone(),
                    description: None,
                    message: "first duplicate".to_string(),
                },
                DescriptorStep::Notify {
                    id: duplicate_id,
                    description: None,
                    message: "second duplicate".to_string(),
                },
            ],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "duplicate top-level step ids must be rejected"
        );
    }

    #[test]
    fn duplicate_nested_step_ids_fail_validation(
        name in "[a-z_]{3,20}",
        duplicate_id in "[a-z_]{3,15}",
    ) {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name,
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "branch".to_string(),
                description: None,
                test_text: "matched".to_string(),
                matcher: DescriptorMatcher::Substring {
                    value: "matched".to_string(),
                },
                then_steps: vec![DescriptorStep::Log {
                    id: duplicate_id.clone(),
                    description: None,
                    message: "then duplicate".to_string(),
                }],
                else_steps: vec![DescriptorStep::Notify {
                    id: duplicate_id,
                    description: None,
                    message: "else duplicate".to_string(),
                }],
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "duplicate nested step ids must be rejected"
        );
    }

    #[test]
    fn empty_nested_step_id_fails_validation(
        name in "[a-z_]{3,20}",
    ) {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name,
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Loop {
                id: "outer_loop".to_string(),
                description: None,
                count: 1,
                body: vec![DescriptorStep::Log {
                    id: "   ".to_string(),
                    description: None,
                    message: "empty nested id".to_string(),
                }],
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "empty nested step ids must be rejected"
        );
    }

    #[test]
    fn send_text_step_respects_configured_text_length_limit(
        max_text_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "s".to_string(),
                description: None,
                text: "x".repeat(max_text_len),
                wait_for: None,
                wait_timeout_ms: None,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "send_text at max_text_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "s".to_string(),
                description: None,
                text: "x".repeat(max_text_len.saturating_add(1)),
                wait_for: None,
                wait_timeout_ms: None,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "send_text longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn empty_send_text_step_fails_validation() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "send_text_blank".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "s".to_string(),
                description: None,
                text: " \t ".to_string(),
                wait_for: None,
                wait_timeout_ms: None,
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "send_text steps must inject non-empty text"
        );
    }

    #[test]
    fn conditional_test_text_respects_configured_text_length_limit(
        max_text_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "b".to_string(),
                description: None,
                test_text: "x".repeat(max_text_len),
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                then_steps: vec![DescriptorStep::Log {
                    id: "t".to_string(),
                    description: None,
                    message: "matched".to_string(),
                }],
                else_steps: Vec::new(),
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "conditional test_text at max_text_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "x".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "b".to_string(),
                description: None,
                test_text: "x".repeat(max_text_len.saturating_add(1)),
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                then_steps: vec![DescriptorStep::Log {
                    id: "t".to_string(),
                    description: None,
                    message: "matched".to_string(),
                }],
                else_steps: Vec::new(),
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "conditional test_text longer than max_text_len should fail validation"
        );
    }

    #[test]
    fn empty_conditional_test_text_fails_validation() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "conditional_blank".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "b".to_string(),
                description: None,
                test_text: " \t ".to_string(),
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                then_steps: vec![DescriptorStep::Log {
                    id: "t".to_string(),
                    description: None,
                    message: "matched".to_string(),
                }],
                else_steps: Vec::new(),
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "conditional steps must test non-empty text"
        );
    }

    #[test]
    fn empty_conditional_then_steps_fails_validation() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "conditional_empty_then".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "b".to_string(),
                description: None,
                test_text: "x".to_string(),
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                then_steps: Vec::new(),
                else_steps: vec![DescriptorStep::Log {
                    id: "else".to_string(),
                    description: None,
                    message: "not matched".to_string(),
                }],
            }],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "conditional then_steps must be non-empty to avoid no-op control flow"
        );
    }

    #[test]
    fn nested_steps_count_against_configured_step_limit(
        max_steps in 2usize..16,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_steps,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit_body = (0..max_steps.saturating_sub(1))
            .map(|idx| DescriptorStep::Log {
                id: format!("at_child_{idx}"),
                description: None,
                message: "child".to_string(),
            })
            .collect();
        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "nested_steps_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Loop {
                id: "root_loop".to_string(),
                description: None,
                count: 1,
                body: at_limit_body,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "nested descriptor with exactly max_steps total nodes should validate"
        );

        let over_limit_body = (0..max_steps)
            .map(|idx| DescriptorStep::Log {
                id: format!("over_child_{idx}"),
                description: None,
                message: "child".to_string(),
            })
            .collect();
        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "nested_steps_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Loop {
                id: "root_loop".to_string(),
                description: None,
                count: 1,
                body: over_limit_body,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "nested descriptor with more than max_steps total nodes should fail validation"
        );
    }

    #[test]
    fn wait_for_timeout_respects_configured_limit(
        max_wait_timeout_ms in 1u64..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_wait_timeout_ms,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "wait_timeout_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                timeout_ms: Some(max_wait_timeout_ms),
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "wait_for timeout at max_wait_timeout_ms should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "wait_timeout_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                timeout_ms: Some(max_wait_timeout_ms.saturating_add(1)),
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "wait_for timeout longer than max_wait_timeout_ms should fail validation"
        );
    }

    #[test]
    fn sleep_duration_respects_configured_limit(
        max_sleep_ms in 1u64..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_sleep_ms,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "sleep_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Sleep {
                id: "sleep".to_string(),
                description: None,
                duration_ms: max_sleep_ms,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "sleep duration at max_sleep_ms should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "sleep_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Sleep {
                id: "sleep".to_string(),
                description: None,
                duration_ms: max_sleep_ms.saturating_add(1),
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "sleep duration longer than max_sleep_ms should fail validation"
        );
    }

    #[test]
    fn send_text_wait_timeout_respects_configured_limit(
        max_wait_timeout_ms in 1u64..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_wait_timeout_ms,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "send_wait_timeout_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "send".to_string(),
                description: None,
                text: "x".to_string(),
                wait_for: Some(DescriptorMatcher::Substring {
                    value: "x".to_string(),
                }),
                wait_timeout_ms: Some(max_wait_timeout_ms),
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "send_text wait_timeout_ms at max_wait_timeout_ms should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "send_wait_timeout_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "send".to_string(),
                description: None,
                text: "x".to_string(),
                wait_for: Some(DescriptorMatcher::Substring {
                    value: "x".to_string(),
                }),
                wait_timeout_ms: Some(max_wait_timeout_ms.saturating_add(1)),
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "send_text wait_timeout_ms longer than max_wait_timeout_ms should fail validation"
        );
    }

    #[test]
    fn send_text_wait_timeout_without_wait_for_fails_validation(timeout_ms in 1u64..120_000) {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "send_wait_timeout_without_wait_for".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "send".to_string(),
                description: None,
                text: "x".to_string(),
                wait_for: None,
                wait_timeout_ms: Some(timeout_ms),
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();

        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "send_text wait_timeout_ms must be paired with a wait_for matcher"
        );
    }

    #[test]
    fn zero_timing_values_fail_validation(step_variant in 0u8..3) {
        let step = match step_variant {
            0 => DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                timeout_ms: Some(0),
            },
            1 => DescriptorStep::Sleep {
                id: "sleep".to_string(),
                description: None,
                duration_ms: 0,
            },
            _ => DescriptorStep::SendText {
                id: "send".to_string(),
                description: None,
                text: "x".to_string(),
                wait_for: Some(DescriptorMatcher::Substring {
                    value: "x".to_string(),
                }),
                wait_timeout_ms: Some(0),
            },
        };
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "zero_timing".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![step],
            on_failure: None,
        };

        let limits = frankenterm_core::workflows::DescriptorLimits::default();
        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "workflow timing fields must be positive when explicitly configured"
        );
    }

    #[test]
    fn substring_matcher_respects_configured_match_length_limit(
        max_match_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_match_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "substring_match_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Substring {
                    value: "x".repeat(max_match_len),
                },
                timeout_ms: None,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "substring matcher at max_match_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "substring_match_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Substring {
                    value: "x".repeat(max_match_len.saturating_add(1)),
                },
                timeout_ms: None,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "substring matcher longer than max_match_len should fail validation"
        );
    }

    #[test]
    fn regex_matcher_respects_configured_match_length_limit(
        max_match_len in 1usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_match_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "regex_match_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Regex {
                    pattern: "x".repeat(max_match_len),
                },
                timeout_ms: None,
            }],
            on_failure: None,
        };
        prop_assert!(
            at_limit.validate(&limits).is_ok(),
            "regex matcher at max_match_len should validate"
        );

        let over_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "regex_match_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Regex {
                    pattern: "x".repeat(max_match_len.saturating_add(1)),
                },
                timeout_ms: None,
            }],
            on_failure: None,
        };
        prop_assert!(
            over_limit.validate(&limits).is_err(),
            "regex matcher longer than max_match_len should fail validation"
        );
    }

    #[test]
    fn empty_matchers_fail_validation(matcher_variant in 0u8..2) {
        let matcher = match matcher_variant {
            0 => DescriptorMatcher::Substring {
                value: " \t ".to_string(),
            },
            _ => DescriptorMatcher::Regex {
                pattern: " \t ".to_string(),
            },
        };
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "empty_matcher".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher,
                timeout_ms: None,
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();

        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "descriptor validation must reject empty matchers"
        );
    }

    #[test]
    fn regex_matchers_that_match_empty_input_fail_validation() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "empty_regex_matcher".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::WaitFor {
                id: "wait".to_string(),
                description: None,
                matcher: DescriptorMatcher::Regex {
                    pattern: ".*".to_string(),
                },
                timeout_ms: None,
            }],
            on_failure: None,
        };
        let limits = frankenterm_core::workflows::DescriptorLimits::default();

        prop_assert!(
            descriptor.validate(&limits).is_err(),
            "descriptor validation must reject regex matchers that match empty input"
        );
    }

    #[test]
    fn descriptor_matcher_substring_serde_contains_kind(value in "[a-z ]{3,20}") {
        let matcher = DescriptorMatcher::Substring { value };
        let json = serde_json::to_string(&matcher).unwrap();
        prop_assert!(json.contains("\"kind\":\"substring\""));
    }

    #[test]
    fn descriptor_matcher_regex_serde_contains_kind(pattern in "[a-z]+") {
        let matcher = DescriptorMatcher::Regex { pattern };
        let json = serde_json::to_string(&matcher).unwrap();
        prop_assert!(json.contains("\"kind\":\"regex\""));
    }

    #[test]
    fn descriptor_step_types_serialize_to_correct_tag(step in arb_leaf_step()) {
        let json = serde_json::to_string(&step).unwrap();
        match &step {
            DescriptorStep::WaitFor { .. } => prop_assert!(json.contains("\"type\":\"wait_for\"")),
            DescriptorStep::Sleep { .. } => prop_assert!(json.contains("\"type\":\"sleep\"")),
            DescriptorStep::SendText { .. } => prop_assert!(json.contains("\"type\":\"send_text\"")),
            DescriptorStep::SendCtrl { .. } => prop_assert!(json.contains("\"type\":\"send_ctrl\"")),
            DescriptorStep::Notify { .. } => prop_assert!(json.contains("\"type\":\"notify\"")),
            DescriptorStep::Log { .. } => prop_assert!(json.contains("\"type\":\"log\"")),
            DescriptorStep::Abort { .. } => prop_assert!(json.contains("\"type\":\"abort\"")),
            DescriptorStep::Conditional { .. } => prop_assert!(json.contains("\"type\":\"conditional\"")),
            DescriptorStep::Loop { .. } => prop_assert!(json.contains("\"type\":\"loop\"")),
        }
    }

    #[test]
    fn control_key_all_variants_serialize(key in arb_descriptor_control_key()) {
        let json = serde_json::to_string(&key).unwrap();
        let expected = match key {
            DescriptorControlKey::CtrlC => "\"ctrl_c\"",
            DescriptorControlKey::CtrlD => "\"ctrl_d\"",
            DescriptorControlKey::CtrlZ => "\"ctrl_z\"",
        };
        prop_assert_eq!(json, expected);
    }
}
