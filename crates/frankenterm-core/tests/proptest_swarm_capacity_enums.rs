//! Property tests for the stable public decision enums of the swarm
//! capacity-admission subsystem in `frankenterm_core::runtime_telemetry`.
//!
//! These enums are part of the controller's serialized surface (operator
//! plans / audit records) but had no coverage. Pins serde round-trips and
//! the stability/uniqueness of the `as_str()` labels.

use proptest::prelude::*;

use frankenterm_core::runtime_telemetry::{
    SwarmCapacityAdmissionControllerMode, SwarmCapacityBudgetUnit, SwarmCapacityDecisionAction,
};

const MODES: &[SwarmCapacityAdmissionControllerMode] = &[
    SwarmCapacityAdmissionControllerMode::Disabled,
    SwarmCapacityAdmissionControllerMode::DryRun,
    SwarmCapacityAdmissionControllerMode::ExpectedLoss,
    SwarmCapacityAdmissionControllerMode::Enabled,
];

const ACTIONS: &[SwarmCapacityDecisionAction] = &[
    SwarmCapacityDecisionAction::Allow,
    SwarmCapacityDecisionAction::ReduceAdmission,
    SwarmCapacityDecisionAction::BlockAdmission,
];

const UNITS: &[SwarmCapacityBudgetUnit] = &[
    SwarmCapacityBudgetUnit::Slots,
    SwarmCapacityBudgetUnit::Processes,
    SwarmCapacityBudgetUnit::Bytes,
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn controller_mode_serde_roundtrip(i in 0usize..MODES.len()) {
        let mode = MODES[i];
        let back: SwarmCapacityAdmissionControllerMode =
            serde_json::from_str(&serde_json::to_string(&mode).unwrap()).unwrap();
        prop_assert_eq!(mode, back);
        prop_assert!(!mode.as_str().is_empty());
    }

    #[test]
    fn decision_action_serde_roundtrip(i in 0usize..ACTIONS.len()) {
        let action = ACTIONS[i];
        let back: SwarmCapacityDecisionAction =
            serde_json::from_str(&serde_json::to_string(&action).unwrap()).unwrap();
        prop_assert_eq!(action, back);
        prop_assert!(!action.as_str().is_empty());
    }

    #[test]
    fn budget_unit_serde_roundtrip(i in 0usize..UNITS.len()) {
        let unit = UNITS[i];
        let back: SwarmCapacityBudgetUnit =
            serde_json::from_str(&serde_json::to_string(&unit).unwrap()).unwrap();
        prop_assert_eq!(unit, back);
    }
}

#[test]
fn controller_mode_as_str_labels_are_unique() {
    let labels: Vec<&str> = MODES.iter().map(|m| m.as_str()).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len(), "controller mode labels must be unique");
}

#[test]
fn decision_action_as_str_labels_are_unique() {
    let labels: Vec<&str> = ACTIONS.iter().map(|a| a.as_str()).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len(), "decision action labels must be unique");
}
