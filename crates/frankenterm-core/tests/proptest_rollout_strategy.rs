use proptest::prelude::*;

use frankenterm_core::rollout_strategy::{
    FeatureRolloutRegistry, FeatureTimeline, Marker, RolloutPhase, RolloutState,
    TransitionValidity, transition_validity,
};

fn arb_phase() -> impl Strategy<Value = RolloutPhase> {
    prop_oneof![
        Just(RolloutPhase::Hidden),
        Just(RolloutPhase::OptIn),
        Just(RolloutPhase::Default),
        Just(RolloutPhase::Cleanup),
    ]
}

fn arb_marker() -> impl Strategy<Value = Marker> {
    prop_oneof![
        Just(Marker::M0),
        Just(Marker::M1),
        Just(Marker::M2),
        Just(Marker::M3),
        Just(Marker::M4),
        Just(Marker::M5),
        Just(Marker::M6),
        Just(Marker::Future),
    ]
}

fn marker_from_index(index: u8) -> Marker {
    match index {
        0 => Marker::M0,
        1 => Marker::M1,
        2 => Marker::M2,
        3 => Marker::M3,
        4 => Marker::M4,
        5 => Marker::M5,
        6 => Marker::M6,
        _ => Marker::Future,
    }
}

fn arb_ordered_timeline() -> impl Strategy<Value = FeatureTimeline> {
    prop::array::uniform4(0u8..=7).prop_map(|mut indexes| {
        indexes.sort();
        FeatureTimeline {
            hidden_at: marker_from_index(indexes[0]),
            opt_in_at: marker_from_index(indexes[1]),
            default_at: marker_from_index(indexes[2]),
            cleanup_at: marker_from_index(indexes[3]),
        }
    })
}

fn expected_transition(current: RolloutPhase, next: RolloutPhase) -> TransitionValidity {
    if current == next {
        return TransitionValidity::NoOp;
    }
    match (current, next) {
        (RolloutPhase::Hidden, RolloutPhase::OptIn)
        | (RolloutPhase::OptIn, RolloutPhase::Default)
        | (RolloutPhase::Default, RolloutPhase::Cleanup) => TransitionValidity::Forward,
        (RolloutPhase::OptIn, RolloutPhase::Hidden)
        | (RolloutPhase::Default, RolloutPhase::OptIn) => TransitionValidity::EmergencyRollback,
        _ => TransitionValidity::Illegal,
    }
}

fn expected_phase_at(timeline: FeatureTimeline, marker: Marker) -> RolloutPhase {
    let ordinal = marker.ordinal();
    if ordinal >= timeline.cleanup_at.ordinal() {
        RolloutPhase::Cleanup
    } else if ordinal >= timeline.default_at.ordinal() {
        RolloutPhase::Default
    } else if ordinal >= timeline.opt_in_at.ordinal() {
        RolloutPhase::OptIn
    } else {
        RolloutPhase::Hidden
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_rollout_strategy_phase_predicates_match_lifecycle_order(
        phase in arb_phase(),
    ) {
        prop_assert_eq!(
            phase.is_enabled_by_default(),
            matches!(phase, RolloutPhase::Default | RolloutPhase::Cleanup),
        );
        prop_assert_eq!(
            phase.is_runtime_toggleable(),
            matches!(phase, RolloutPhase::OptIn | RolloutPhase::Default),
        );
        prop_assert_eq!(
            phase.has_legacy_fallback(),
            matches!(phase, RolloutPhase::OptIn | RolloutPhase::Default),
        );

        if phase == RolloutPhase::Cleanup {
            prop_assert!(phase.is_enabled_by_default());
            prop_assert!(!phase.is_runtime_toggleable());
            prop_assert!(!phase.has_legacy_fallback());
        }
    }

    #[test]
    fn proptest_rollout_strategy_transition_matrix_is_exact(
        current in arb_phase(),
        next in arb_phase(),
    ) {
        let validity = transition_validity(current, next);
        prop_assert_eq!(validity, expected_transition(current, next));
        prop_assert_eq!(validity.is_legal(), !matches!(validity, TransitionValidity::Illegal));
    }

    #[test]
    fn proptest_rollout_strategy_transition_to_mutates_only_for_real_legal_transitions(
        current in arb_phase(),
        next in arb_phase(),
    ) {
        let mut state = RolloutState::at("renderer_feature", current);
        let validity = state.transition_to(next);

        prop_assert_eq!(validity, transition_validity(current, next));
        prop_assert_eq!(state.feature_id(), "renderer_feature");
        match validity {
            TransitionValidity::Forward | TransitionValidity::EmergencyRollback => {
                prop_assert_eq!(state.current(), next);
                prop_assert_eq!(state.transitions_applied(), 1);
            }
            TransitionValidity::NoOp | TransitionValidity::Illegal => {
                prop_assert_eq!(state.current(), current);
                prop_assert_eq!(state.transitions_applied(), 0);
            }
        }
    }

    #[test]
    fn proptest_rollout_strategy_ordered_timeline_phase_at_matches_thresholds(
        timeline in arb_ordered_timeline(),
        marker in arb_marker(),
    ) {
        prop_assert_eq!(timeline.phase_at(marker), expected_phase_at(timeline, marker));

        let phase = timeline.phase_at(marker);
        match phase {
            RolloutPhase::Hidden => {
                prop_assert!(marker.ordinal() < timeline.opt_in_at.ordinal()
                    || timeline.opt_in_at.ordinal() == timeline.hidden_at.ordinal());
            }
            RolloutPhase::OptIn => {
                prop_assert!(marker.ordinal() >= timeline.opt_in_at.ordinal());
                prop_assert!(marker.ordinal() < timeline.default_at.ordinal()
                    || timeline.default_at.ordinal() == timeline.opt_in_at.ordinal());
            }
            RolloutPhase::Default => {
                prop_assert!(marker.ordinal() >= timeline.default_at.ordinal());
                prop_assert!(marker.ordinal() < timeline.cleanup_at.ordinal()
                    || timeline.cleanup_at.ordinal() == timeline.default_at.ordinal());
            }
            RolloutPhase::Cleanup => {
                prop_assert!(marker.ordinal() >= timeline.cleanup_at.ordinal());
            }
        }
    }

    #[test]
    fn proptest_rollout_strategy_canonical_registry_lookup_and_phase_projection_are_consistent(
        marker in arb_marker(),
    ) {
        let registry = FeatureRolloutRegistry::canonical();
        prop_assert!(!registry.is_empty());
        prop_assert_eq!(registry.len(), registry.entries().len());

        let phases = registry.phases_at(marker);
        prop_assert_eq!(phases.len(), registry.len());

        for ((feature_id, phase), entry) in phases.iter().zip(registry.entries()) {
            prop_assert_eq!(*feature_id, entry.feature_id);
            prop_assert_eq!(*phase, entry.timeline.phase_at(marker));
            prop_assert_eq!(registry.lookup(feature_id).map(|found| found.feature_id), Some(*feature_id));
        }
        prop_assert!(registry.lookup("__missing_feature__").is_none());
    }
}
