use frankenterm_core::ntm_parity::{
    NtmParityAcceptanceMatrix, NtmParityCommandOutput, NtmParityScenarioStatus,
    build_divergence_report, build_run_summary, evaluate_scenario,
};

fn fixture_output_for(
    scenario: &frankenterm_core::ntm_parity::NtmParityScenario,
) -> NtmParityCommandOutput {
    let (stdout, stderr, exit_code, execution_error) = match scenario.id.as_str() {
        "NTM-PARITY-001" => (
            serde_json::json!({
                "ok": true,
                "data": { "panes": [] }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-002" => (
            serde_json::json!({
                "ok": true,
                "data": { "text": "hello from pane" }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-003" => (
            serde_json::json!({
                "ok": true,
                "data": { "dry_run": true, "preview": "echo parity" }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-004" => (
            serde_json::json!({
                "ok": true,
                "data": { "matched": true }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-005" => (
            serde_json::json!({
                "ok": true,
                "data": { "matches": [] }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-006" => (
            serde_json::json!({
                "ok": true,
                "data": { "events": [] }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-007" => (
            serde_json::json!({
                "ok": true,
                "data": { "rules": [] }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-008" => (
            serde_json::json!({
                "ok": true,
                "data": { "matches": [] }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-009" => (
            serde_json::json!({
                "ok": true,
                "data": { "snapshot_id": "snap-123" }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-010" => (
            serde_json::json!({
                "ok": true,
                "data": { "sessions": [] }
            })
            .to_string(),
            String::new(),
            Some(0),
            None,
        ),
        "NTM-PARITY-011" => (
            serde_json::json!({
                "ok": false,
                "error": { "code": "robot.require_approval" }
            })
            .to_string(),
            String::new(),
            Some(1),
            None,
        ),
        "NTM-PARITY-012" => (
            "ok=true data={panes=[]}".to_string(),
            "tokens saved: 42".to_string(),
            Some(0),
            None,
        ),
        other => (
            serde_json::json!({
                "ok": false,
                "error": { "code": "test.synthetic_fixture_missing", "scenario_id": other }
            })
            .to_string(),
            String::new(),
            None,
            Some(format!("missing synthetic fixture output for {other}")),
        ),
    };

    let expanded_command = scenario.ft_command.replace("<pane_id>", "0");
    NtmParityCommandOutput {
        scenario_id: scenario.id.clone(),
        command: scenario.ft_command.clone(),
        expanded_command,
        exit_code,
        duration_ms: 5,
        stdout,
        stderr,
        execution_error,
    }
}

#[test]
fn full_ntm_parity_fixture_bundle_passes_with_synthetic_outputs() {
    let corpus = frankenterm_core::ntm_parity::NtmParityCorpus::from_json_str(include_str!(
        "../../../fixtures/e2e/ntm_parity/corpus.v1.json"
    ))
    .expect("corpus fixture should parse");
    let matrix = NtmParityAcceptanceMatrix::from_json_str(include_str!(
        "../../../fixtures/e2e/ntm_parity/acceptance_matrix.v1.json"
    ))
    .expect("acceptance matrix fixture should parse");

    let results = corpus
        .scenarios
        .iter()
        .map(|scenario| {
            let output = fixture_output_for(scenario);
            evaluate_scenario(
                scenario,
                &output,
                vec![format!("scenarios/{}.json", scenario.id)],
                None,
            )
        })
        .collect::<Vec<_>>();

    let summary = build_run_summary("synthetic-run", &matrix, &results);
    let divergence = build_divergence_report("synthetic-run", &matrix, &results);

    assert!(
        results
            .iter()
            .all(|result| matches!(result.status, NtmParityScenarioStatus::Pass))
    );
    assert!(summary.overall_passed);
    assert_eq!(summary.pass_count, corpus.scenarios.len());
    assert_eq!(summary.fail_count, 0);
    assert_eq!(summary.intentional_delta_count, 0);
    assert_eq!(summary.untested_count, 0);
    assert_eq!(summary.divergence_count, 0);
    assert!(summary.blocking_failures.is_empty());
    assert!(summary.high_priority_failures.is_empty());
    assert!(summary.envelope_violations.is_empty());
    assert_eq!(divergence.total_divergences, 0);
    assert!(divergence.divergences.is_empty());
}
