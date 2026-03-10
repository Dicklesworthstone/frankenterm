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
