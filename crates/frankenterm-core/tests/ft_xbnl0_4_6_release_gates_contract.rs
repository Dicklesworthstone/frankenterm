use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn contract_path() -> PathBuf {
    workspace_root().join("docs/ft-xbnl0-4-6-release-gates.json")
}

fn load_contract() -> serde_json::Value {
    let path = contract_path();
    let body = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&body)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()))
}

#[test]
fn release_gate_contract_has_named_blocking_gates() {
    let contract = load_contract();
    let gates = contract["gate_map"].as_array().expect("gate_map array");
    let gate_ids: Vec<&str> = gates
        .iter()
        .map(|gate| gate["gate_id"].as_str().expect("gate_id"))
        .collect();

    assert_eq!(
        gate_ids,
        vec![
            "REL-01-leak-oracle",
            "REL-02-guard-surface",
            "REL-03-soak-confidence",
            "REL-04-performance-budget"
        ],
        "ft-xbnl0.4.6 must keep the canonical leak, soak, and performance gates",
    );

    for gate in gates {
        assert_eq!(
            gate["blocking"].as_bool(),
            Some(true),
            "all ft-xbnl0.4.6 release gates should be blocking"
        );
        assert!(
            !gate["name"].as_str().unwrap_or_default().is_empty(),
            "release gate names must be non-empty"
        );
        assert!(
            !gate["category"].as_str().unwrap_or_default().is_empty(),
            "release gate categories must be non-empty"
        );
    }
}

#[test]
fn soak_and_performance_thresholds_match_finish_line_profiles() {
    let contract = load_contract();
    let thresholds = &contract["thresholds"];

    assert_eq!(thresholds["min_smoke_cycles"].as_u64(), Some(1));
    assert_eq!(thresholds["min_release_cycles"].as_u64(), Some(3));
    assert_eq!(
        thresholds["required_pane_scales"]
            .as_array()
            .expect("pane scales")
            .iter()
            .map(|value| value.as_u64().expect("u64"))
            .collect::<Vec<_>>(),
        vec![1, 50, 100, 200]
    );
    assert_eq!(
        thresholds["required_metric_count"].as_u64(),
        Some(8),
        "the performance gate should require all eight swarm-stress metrics"
    );
    assert_eq!(
        thresholds["required_backpressure_tiers"]
            .as_array()
            .expect("backpressure tiers")
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>(),
        vec!["Black"]
    );

    assert_eq!(thresholds["max_peak_rss_mb"].as_f64(), Some(32.0));
    assert_eq!(thresholds["max_duration_s"].as_f64(), Some(3.0));
}

#[test]
fn release_gate_contract_keeps_actionable_diagnostics() {
    let contract = load_contract();
    let gates = contract["gate_map"].as_array().expect("gate_map array");
    let artifact_roots = contract["artifact_roots"]
        .as_object()
        .expect("artifact_roots object");

    assert!(
        artifact_roots.contains_key("leak_oracle_root"),
        "contract must point to the upstream leak-oracle artifact root"
    );
    assert!(
        artifact_roots.contains_key("soak_matrix_root"),
        "contract must point to the upstream soak-matrix artifact root"
    );
    assert!(
        artifact_roots.contains_key("guard_report"),
        "contract must point to the permanent guard report"
    );

    for gate in gates {
        assert!(
            !gate["source"].as_str().unwrap_or_default().is_empty(),
            "{} must cite the evidence source it consumes",
            gate["gate_id"].as_str().unwrap_or("<missing>")
        );
        assert!(
            !gate["failure_action"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{} must point operators to a real remediation action",
            gate["gate_id"].as_str().unwrap_or("<missing>")
        );
    }
}
