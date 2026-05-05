//! `frankenterm-core-replay` — replay sub-crate extracted from
//! `frankenterm-core` (ft-y0loj.4 / ft-j1qjt.2).
//!
//! See `Cargo.toml` for the precise scope. Only modules with **zero
//! inbound references from non-replay core** were moved; the three
//! bridge modules (`replay`, `replay_capture`, `replay_fixture_harvest`)
//! stay in core because policy.rs / runtime.rs / workflows / recorder_replay
//! reach into them. Their full extraction is filed as ft-j1qjt.2.x and
//! requires `event_id` / `ingest::CapturedSegment` / `recording`
//! extracted to leaves first.

pub mod replay_artifact_registry;
pub mod replay_capture_tiering;
pub mod replay_checkpoint;
pub mod replay_ci_gate;
pub mod replay_cli;
pub mod replay_counterfactual;
pub mod replay_decision_diff;
pub mod replay_fault_injection;
pub mod replay_guardrails;
pub mod replay_guardrails_gate;
pub mod replay_guide;
pub mod replay_io_heatmap;
pub mod replay_mcp;
pub mod replay_merge;
pub mod replay_performance;
pub mod replay_post_incident;
pub mod replay_provenance;
pub mod replay_remediation;
pub mod replay_report;
pub mod replay_risk_scoring;
pub mod replay_robot;
pub mod replay_scenario_matrix;
pub mod replay_shadow_rollout;
pub mod replay_side_effect_barrier;
pub mod replay_test_orchestrator;
pub mod replay_usability_pilot;

#[cfg(test)]
mod tests {
    use frankenterm_core_replay_types::replay_decision_graph::{DecisionEvent, DecisionType};

    fn fixture_sequence(run_id: &str, reverse_output_keys: bool) -> Vec<DecisionEvent> {
        let output = if reverse_output_keys {
            serde_json::json!({
                "z": [3, 2, 1],
                "m": {"right": true, "left": false},
                "a": "done",
            })
        } else {
            serde_json::json!({
                "a": "done",
                "m": {"left": false, "right": true},
                "z": [3, 2, 1],
            })
        };

        let mut first = DecisionEvent::new(
            DecisionType::PatternMatch,
            11,
            "codex.usage.reached",
            "anchor=Usage limit reached;version=3",
            "pane=11\nUsage limit reached. Try again later.",
            output.clone(),
            Some("input-event-1".to_string()),
            Some(0.98),
            42_000,
        );
        first.replay_run_id = run_id.to_string();
        first.wall_clock_ms = if reverse_output_keys {
            900_002
        } else {
            900_001
        };

        let mut second = DecisionEvent::new(
            DecisionType::WorkflowStep,
            11,
            "handle_usage_limit.refresh",
            "workflow_step=refresh;version=1",
            "decision=usage-limit;pane=11",
            output,
            Some("input-event-2".to_string()),
            Some(1.0),
            42_100,
        );
        second.replay_run_id = run_id.to_string();
        second.wall_clock_ms = if reverse_output_keys {
            900_004
        } else {
            900_003
        };

        vec![first, second]
    }

    #[test]
    fn decision_event_hash_stable_across_runs() {
        let run_a = fixture_sequence("run-a", false);
        let run_b = fixture_sequence("run-b", true);

        let hashes_a: Vec<_> = run_a
            .iter()
            .map(|event| {
                (
                    event.definition_hash.as_str(),
                    event.input_hash.as_str(),
                    event.output_hash.as_str(),
                )
            })
            .collect();
        let hashes_b: Vec<_> = run_b
            .iter()
            .map(|event| {
                (
                    event.definition_hash.as_str(),
                    event.input_hash.as_str(),
                    event.output_hash.as_str(),
                )
            })
            .collect();

        assert_eq!(hashes_a, hashes_b);
        assert!(
            hashes_a
                .iter()
                .all(|(definition, input, output)| definition.len() == 64
                    && input.len() == 64
                    && output.len() == 64)
        );
    }
}
