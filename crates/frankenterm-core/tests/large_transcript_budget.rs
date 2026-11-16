use frankenterm_core::large_swarm_replay::{
    LargeSwarmRegressionThresholds, LargeSwarmScenario, evaluate_large_swarm_thresholds,
    generate_large_swarm_corpus, summarize_large_swarm_replay,
};
use serde_json::json;
use std::time::Instant;

#[test]
fn terminal_conformance_large_transcript_budget() {
    let mut rows = Vec::new();

    for scenario in LargeSwarmScenario::required_scale_points() {
        let started = Instant::now();
        let corpus = generate_large_swarm_corpus(&scenario).expect("generate corpus");
        let summary = summarize_large_swarm_replay(&corpus).expect("summary");
        let elapsed_ms = duration_ms(started);
        let thresholds = LargeSwarmRegressionThresholds::for_scenario(&scenario);
        let verdict = evaluate_large_swarm_thresholds(&summary, &thresholds);
        let corpus_artifact_bytes =
            usize_to_u64(serde_json::to_vec(&corpus).expect("serialize corpus").len());
        let summary_artifact_bytes = usize_to_u64(
            serde_json::to_vec(&summary)
                .expect("serialize summary")
                .len(),
        );
        let total_artifact_bytes = corpus_artifact_bytes.saturating_add(summary_artifact_bytes);
        let memory_proxy_bytes = total_artifact_bytes
            .saturating_add(summary.event_count.saturating_mul(256))
            .saturating_add(summary.pane_count.saturating_mul(512));
        let wall_time_budget_ms = summary.event_count.saturating_mul(10).max(1_000);
        let artifact_size_budget_bytes = summary.event_count.saturating_mul(4_096).max(64 * 1024);
        let memory_proxy_budget_bytes = summary.event_count.saturating_mul(8_192).max(128 * 1024);

        let mut failures = Vec::new();
        failures.extend(
            verdict
                .diffs
                .iter()
                .map(|diff| format!("{}:{}", diff.field, diff.message)),
        );
        if elapsed_ms > wall_time_budget_ms {
            failures.push(format!("wall_time_ms:{elapsed_ms} > {wall_time_budget_ms}"));
        }
        if total_artifact_bytes > artifact_size_budget_bytes {
            failures.push(format!(
                "artifact_bytes:{total_artifact_bytes} > {artifact_size_budget_bytes}"
            ));
        }
        if memory_proxy_bytes > memory_proxy_budget_bytes {
            failures.push(format!(
                "memory_proxy_bytes:{memory_proxy_bytes} > {memory_proxy_budget_bytes}"
            ));
        }

        let outcome = if failures.is_empty() {
            "passed"
        } else {
            "failed"
        };
        let row = json!({
            "component": "terminal_conformance.large_transcript_budget",
            "bead_id": "ft-hme39.5",
            "scenario_id": scenario.scenario_id.clone(),
            "pane_count": scenario.pane_count,
            "event_count": summary.event_count,
            "wall_time_ms": elapsed_ms,
            "artifact_bytes": total_artifact_bytes,
            "memory_proxy_bytes": memory_proxy_bytes,
            "max_events_per_pane": summary.max_events_per_pane,
            "output_bytes": summary.output_bytes,
            "summary_digest": summary.summary_digest,
            "budgets": {
                "max_event_count": thresholds.max_event_count,
                "max_duration_ms": thresholds.max_duration_ms,
                "max_output_bytes": thresholds.max_output_bytes,
                "max_events_per_pane": thresholds.max_events_per_pane,
                "max_wall_time_ms": wall_time_budget_ms,
                "max_artifact_bytes": artifact_size_budget_bytes,
                "max_memory_proxy_bytes": memory_proxy_budget_bytes
            },
            "outcome": outcome,
            "reason_codes": failures
        });
        println!("{row}");
        rows.push(row);
    }

    let failed = rows
        .iter()
        .filter(|row| row.get("outcome").and_then(serde_json::Value::as_str) == Some("failed"))
        .count();
    let summary = json!({
        "component": "terminal_conformance.large_transcript_budget",
        "bead_id": "ft-hme39.5",
        "scenario_id": "terminal-conformance-large-transcript-budget",
        "scale_point_count": rows.len(),
        "failed_count": failed,
        "outcome": if failed == 0 { "passed" } else { "failed" }
    });
    println!("{summary}");

    assert_eq!(
        failed, 0,
        "large transcript budget failed; see JSON metric rows above"
    );
}

fn duration_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
