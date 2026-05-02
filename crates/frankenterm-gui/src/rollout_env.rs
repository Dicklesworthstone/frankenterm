use frankenterm_core::rollout_strategy::{
    FeatureRolloutRegistry, Marker, RolloutPhase, RolloutState, TransitionValidity,
};
use std::env;

const FT_RENDERER_PREFIX: &str = "FT_RENDERER_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererRolloutReport {
    pub states: Vec<RendererRolloutState>,
    pub overrides: Vec<RendererRolloutOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererRolloutState {
    pub feature_id: &'static str,
    pub display_name: &'static str,
    pub env_var: String,
    pub canonical_phase: RolloutPhase,
    pub effective_phase: RolloutPhase,
    pub transitions_applied: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RendererRolloutOverride {
    pub feature_id: &'static str,
    pub env_var: String,
    pub raw_value: String,
    pub before: RolloutPhase,
    pub requested: Option<RolloutPhase>,
    pub validity: Option<TransitionValidity>,
    pub after: RolloutPhase,
}

#[must_use]
pub fn resolve_canonical_renderer_rollouts_from_env(marker: Marker) -> RendererRolloutReport {
    let registry = FeatureRolloutRegistry::canonical();
    resolve_renderer_rollouts(&registry, marker, |name| env::var(name).ok())
}

#[must_use]
pub fn resolve_renderer_rollouts<F>(
    registry: &FeatureRolloutRegistry,
    marker: Marker,
    mut env_lookup: F,
) -> RendererRolloutReport
where
    F: FnMut(&str) -> Option<String>,
{
    let mut states = Vec::with_capacity(registry.len());
    let mut overrides = Vec::new();

    for entry in registry.entries() {
        let canonical_phase = entry.timeline.phase_at(marker);
        let env_var = renderer_env_var(entry.feature_id);
        let mut state = RolloutState::at(entry.feature_id, canonical_phase);

        if let Some(raw_value) = env_lookup(&env_var) {
            let before = state.current();
            let requested = renderer_env_requested_phase(before, &raw_value);
            let validity = requested.map(|phase| state.transition_to(phase));

            overrides.push(RendererRolloutOverride {
                feature_id: entry.feature_id,
                env_var: env_var.clone(),
                raw_value,
                before,
                requested,
                validity,
                after: state.current(),
            });
        }

        states.push(RendererRolloutState {
            feature_id: entry.feature_id,
            display_name: entry.display_name,
            env_var,
            canonical_phase,
            effective_phase: state.current(),
            transitions_applied: state.transitions_applied(),
        });
    }

    RendererRolloutReport { states, overrides }
}

#[must_use]
pub fn renderer_env_var(feature_id: &str) -> String {
    let mut name = String::with_capacity(FT_RENDERER_PREFIX.len() + feature_id.len());
    name.push_str(FT_RENDERER_PREFIX);

    for ch in feature_id.chars() {
        if ch.is_ascii_alphanumeric() {
            name.push(ch.to_ascii_uppercase());
        } else {
            name.push('_');
        }
    }

    name
}

#[must_use]
pub fn renderer_env_requested_phase(
    current: RolloutPhase,
    raw_value: &str,
) -> Option<RolloutPhase> {
    let value = raw_value.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "0" | "false" | "no" | "off" | "disable" | "disabled" => None,
        "1" | "true" | "yes" | "on" | "enable" | "enabled" | "opt-in" | "opt_in" => {
            Some(match current {
                RolloutPhase::Hidden | RolloutPhase::OptIn => RolloutPhase::OptIn,
                RolloutPhase::Default | RolloutPhase::Cleanup => current,
            })
        }
        "rollback" | "emergency-rollback" | "emergency_rollback" => Some(match current {
            RolloutPhase::Hidden => RolloutPhase::Hidden,
            RolloutPhase::OptIn => RolloutPhase::Hidden,
            RolloutPhase::Default | RolloutPhase::Cleanup => RolloutPhase::OptIn,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::rollout_strategy::{FeatureRolloutEntry, FeatureTimeline};
    use std::collections::HashMap;

    fn custom_registry() -> FeatureRolloutRegistry {
        let mut registry = FeatureRolloutRegistry::new();
        registry.push(FeatureRolloutEntry {
            feature_id: "hidden_feature",
            display_name: "Hidden feature",
            timeline: FeatureTimeline {
                hidden_at: Marker::M0,
                opt_in_at: Marker::M1,
                default_at: Marker::M2,
                cleanup_at: Marker::M3,
            },
            source_bead: "test",
        });
        registry.push(FeatureRolloutEntry {
            feature_id: "default_feature",
            display_name: "Default feature",
            timeline: FeatureTimeline {
                hidden_at: Marker::M0,
                opt_in_at: Marker::M0,
                default_at: Marker::M0,
                cleanup_at: Marker::M3,
            },
            source_bead: "test",
        });
        registry.push(FeatureRolloutEntry {
            feature_id: "cleanup_feature",
            display_name: "Cleanup feature",
            timeline: FeatureTimeline {
                hidden_at: Marker::M0,
                opt_in_at: Marker::M0,
                default_at: Marker::M0,
                cleanup_at: Marker::M0,
            },
            source_bead: "test",
        });
        registry
    }

    #[test]
    fn renderer_env_var_normalizes_feature_ids() {
        assert_eq!(
            renderer_env_var("atlas_stability"),
            "FT_RENDERER_ATLAS_STABILITY"
        );
        assert_eq!(
            renderer_env_var("metal-direct.v2"),
            "FT_RENDERER_METAL_DIRECT_V2"
        );
    }

    #[test]
    fn hidden_feature_can_opt_in_from_env() {
        let registry = custom_registry();
        let env = HashMap::from([("FT_RENDERER_HIDDEN_FEATURE", "1")]);
        let report = resolve_renderer_rollouts(&registry, Marker::M0, |name| {
            env.get(name).map(ToString::to_string)
        });

        let state = report
            .states
            .iter()
            .find(|state| state.feature_id == "hidden_feature")
            .expect("hidden feature state");

        assert_eq!(state.canonical_phase, RolloutPhase::Hidden);
        assert_eq!(state.effective_phase, RolloutPhase::OptIn);
        assert_eq!(state.transitions_applied, 1);
        assert_eq!(
            report.overrides[0].validity,
            Some(TransitionValidity::Forward)
        );
    }

    #[test]
    fn default_feature_rolls_back_to_opt_in_from_env() {
        let registry = custom_registry();
        let env = HashMap::from([("FT_RENDERER_DEFAULT_FEATURE", "rollback")]);
        let report = resolve_renderer_rollouts(&registry, Marker::M0, |name| {
            env.get(name).map(ToString::to_string)
        });

        let state = report
            .states
            .iter()
            .find(|state| state.feature_id == "default_feature")
            .expect("default feature state");

        assert_eq!(state.canonical_phase, RolloutPhase::Default);
        assert_eq!(state.effective_phase, RolloutPhase::OptIn);
        assert_eq!(
            report.overrides[0].validity,
            Some(TransitionValidity::EmergencyRollback)
        );
    }

    #[test]
    fn cleanup_feature_rejects_rollback_after_legacy_path_is_gone() {
        let registry = custom_registry();
        let env = HashMap::from([("FT_RENDERER_CLEANUP_FEATURE", "rollback")]);
        let report = resolve_renderer_rollouts(&registry, Marker::M0, |name| {
            env.get(name).map(ToString::to_string)
        });

        let state = report
            .states
            .iter()
            .find(|state| state.feature_id == "cleanup_feature")
            .expect("cleanup feature state");

        assert_eq!(state.canonical_phase, RolloutPhase::Cleanup);
        assert_eq!(state.effective_phase, RolloutPhase::Cleanup);
        assert_eq!(state.transitions_applied, 0);
        assert_eq!(
            report.overrides[0].validity,
            Some(TransitionValidity::Illegal)
        );
    }

    #[test]
    fn disabled_or_unknown_values_do_not_request_transition() {
        assert_eq!(
            renderer_env_requested_phase(RolloutPhase::Hidden, "0"),
            None
        );
        assert_eq!(
            renderer_env_requested_phase(RolloutPhase::OptIn, "not-a-rollout-command"),
            None
        );
    }
}
