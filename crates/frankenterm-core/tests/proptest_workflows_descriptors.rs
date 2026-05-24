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
}

fn arb_descriptor_failure_handler() -> impl Strategy<Value = DescriptorFailureHandler> {
    prop_oneof![
        "[a-z ${}_]{5,30}".prop_map(|message| DescriptorFailureHandler::Notify { message }),
        "[a-z ${}_]{5,30}".prop_map(|message| DescriptorFailureHandler::Log { message }),
        "[a-z ${}_]{5,30}".prop_map(|message| DescriptorFailureHandler::Abort { message }),
    ]
}

fn arb_descriptor_matcher() -> impl Strategy<Value = DescriptorMatcher> {
    prop_oneof![
        "[a-z ]{3,20}".prop_map(|value| DescriptorMatcher::Substring { value }),
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
            prop::option::of("[a-z ]{5,20}"),
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
            prop::option::of("[a-z ]{5,20}"),
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
            prop::option::of("[a-z ]{5,20}"),
            "[a-z ]{3,50}",
            prop::option::of(arb_descriptor_matcher()),
            prop::option::of(1000u64..120_000),
        )
            .prop_map(|(id, description, text, wait_for, wait_timeout_ms)| {
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
            prop::option::of("[a-z ]{5,20}"),
            arb_descriptor_control_key()
        )
            .prop_map(|(id, description, key)| DescriptorStep::SendCtrl {
                id,
                description,
                key,
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z ]{5,20}"),
            "[a-z ]{5,50}"
        )
            .prop_map(|(id, description, message)| DescriptorStep::Notify {
                id,
                description,
                message,
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z ]{5,20}"),
            "[a-z ]{5,50}"
        )
            .prop_map(|(id, description, message)| DescriptorStep::Log {
                id,
                description,
                message,
            }),
        (
            "[a-z_]{3,10}",
            prop::option::of("[a-z ]{5,20}"),
            "[a-z ]{5,50}"
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
        prop::option::of("[a-z ]{10,40}"),
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
        max_text_len in 0usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "text_limit_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "send".to_string(),
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
            name: "text_limit_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::SendText {
                id: "send".to_string(),
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
    fn conditional_test_text_respects_configured_text_length_limit(
        max_text_len in 0usize..128,
    ) {
        let limits = frankenterm_core::workflows::DescriptorLimits {
            max_text_len,
            ..frankenterm_core::workflows::DescriptorLimits::default()
        };

        let at_limit = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "condition_text_limit_at_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "branch".to_string(),
                description: None,
                test_text: "x".repeat(max_text_len),
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                then_steps: vec![DescriptorStep::Log {
                    id: "then_log".to_string(),
                    description: None,
                    message: String::new(),
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
            name: "condition_text_limit_over_boundary".to_string(),
            description: None,
            triggers: Vec::new(),
            steps: vec![DescriptorStep::Conditional {
                id: "branch".to_string(),
                description: None,
                test_text: "x".repeat(max_text_len.saturating_add(1)),
                matcher: DescriptorMatcher::Substring {
                    value: "x".to_string(),
                },
                then_steps: vec![DescriptorStep::Log {
                    id: "then_log".to_string(),
                    description: None,
                    message: String::new(),
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
