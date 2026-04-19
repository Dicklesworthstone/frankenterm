use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("workspace root")
}

fn write_json(path: &Path, value: &Value) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).expect("write json");
}

fn create_passing_fixture(root: &Path) -> PathBuf {
    let leak_dir = root.join(
        "tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/leak_oracle_regressions/20260419T170000Z",
    );
    write_json(
        &leak_dir.join("summary.json"),
        &json!({ "status": "passed" }),
    );

    let soak_dir =
        root.join("tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T170500Z");
    let smoke_path = soak_dir.join("smoke_cycle_01/artifacts/fixture-smoke/summary.json");
    let release_paths = [
        soak_dir.join("release_cycle_01/artifacts/fixture-release-01/summary.json"),
        soak_dir.join("release_cycle_02/artifacts/fixture-release-02/summary.json"),
        soak_dir.join("release_cycle_03/artifacts/fixture-release-03/summary.json"),
    ];
    let nested_summary = |path: &Path| {
        write_json(
            path,
            &json!({
                "tests_run": 8,
                "peak_rss_mb": 16.6,
                "max_duration_s": 2.2,
                "highest_backpressure_tier": "Black",
                "pane_scales": [1, 50, 100, 200],
                "metric_names": [
                    "stress_100_panes_idle",
                    "stress_200_panes_active",
                    "stress_200_panes_backpressure",
                    "stress_200_panes_idle",
                    "stress_50_panes_active",
                    "stress_50_panes_idle",
                    "stress_rapid_pane_create_destroy",
                    "stress_single_pane_10mb"
                ]
            }),
        );
    };
    nested_summary(&smoke_path);
    for path in &release_paths {
        nested_summary(path);
    }
    write_json(
        &soak_dir.join("summary.json"),
        &json!({
            "status": "passed",
            "profiles": {
                "smoke": {
                    "cycles": 1,
                    "summary": smoke_path
                },
                "release": {
                    "cycles": 3,
                    "summaries": release_paths
                }
            }
        }),
    );

    let guard_report = root.join("docs/ft-xbnl0-5-2-finish-line-guards-validation.json");
    write_json(&guard_report, &json!({ "status": "passed" }));

    let policy_path = root.join("docs/ft-xbnl0-4-6-release-gates.json");
    write_json(
        &policy_path,
        &json!({
            "contract_id": "ft.xbnl0.4.6.release_gates.v1",
            "artifact_roots": {
                "leak_oracle_root": "tests/e2e/artifacts/goal-line/ft-xbnl0.4.4/leak_oracle_regressions",
                "soak_matrix_root": "tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix",
                "guard_report": "docs/ft-xbnl0-5-2-finish-line-guards-validation.json"
            },
            "thresholds": {
                "required_pane_scales": [1, 50, 100, 200],
                "required_metric_count": 8,
                "min_smoke_cycles": 1,
                "min_release_cycles": 3,
                "max_peak_rss_mb": 32.0,
                "max_duration_s": 3.0,
                "required_backpressure_tiers": ["Black"]
            },
            "gate_map": [
                {
                    "gate_id": "REL-01-leak-oracle",
                    "name": "Leak behavior",
                    "category": "reliability",
                    "blocking": true,
                    "source": "latest leak summary",
                    "failure_action": "rerun leak harness"
                },
                {
                    "gate_id": "REL-02-guard-surface",
                    "name": "Permanent guard surface",
                    "category": "guard surface",
                    "blocking": true,
                    "source": "guard report",
                    "failure_action": "fix permanent guards"
                },
                {
                    "gate_id": "REL-03-soak-confidence",
                    "name": "Soak confidence",
                    "category": "soak confidence",
                    "blocking": true,
                    "source": "latest soak wrapper summary",
                    "failure_action": "rerun soak wrapper"
                },
                {
                    "gate_id": "REL-04-performance-budget",
                    "name": "Performance budget",
                    "category": "performance",
                    "blocking": true,
                    "source": "nested soak summaries",
                    "failure_action": "reduce runtime cost"
                }
            ]
        }),
    );
    policy_path
}

fn run_gate_eval(root: &Path, policy_path: &Path, output_path: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(workspace_root().join("scripts/check_ft_xbnl0_4_6_release_gates.sh"))
        .arg("--root")
        .arg(root)
        .arg("--policy-path")
        .arg(policy_path)
        .arg("--output")
        .arg(output_path)
        .current_dir(workspace_root())
        .output()
        .expect("run release gate evaluator")
}

#[test]
fn release_gate_evaluator_accepts_passing_fixture() {
    let temp = tempfile::tempdir().expect("tempdir");
    let policy_path = create_passing_fixture(temp.path());
    let output_path = temp.path().join("report.json");

    let output = run_gate_eval(temp.path(), &policy_path, &output_path);
    assert!(
        output.status.success(),
        "expected pass fixture to succeed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(output_path).unwrap()).unwrap();
    assert_eq!(report["status"], "passed");
    assert_eq!(report["summary"]["blocking_failed"], 0);
}

#[test]
fn release_gate_evaluator_reports_budget_failure_reason() {
    let temp = tempfile::tempdir().expect("tempdir");
    let policy_path = create_passing_fixture(temp.path());
    let output_path = temp.path().join("report.json");

    let failing_summary = temp.path().join(
        "tests/e2e/artifacts/goal-line/ft-xbnl0.4.5/swarm_soak_matrix/20260419T170500Z/release_cycle_03/artifacts/fixture-release-03/summary.json",
    );
    write_json(
        &failing_summary,
        &json!({
            "tests_run": 8,
            "peak_rss_mb": 48.0,
            "max_duration_s": 4.5,
            "highest_backpressure_tier": "Black",
            "pane_scales": [1, 50, 100, 200],
            "metric_names": [
                "stress_100_panes_idle",
                "stress_200_panes_active",
                "stress_200_panes_backpressure",
                "stress_200_panes_idle",
                "stress_50_panes_active",
                "stress_50_panes_idle",
                "stress_rapid_pane_create_destroy",
                "stress_single_pane_10mb"
            ]
        }),
    );

    let output = run_gate_eval(temp.path(), &policy_path, &output_path);
    assert!(
        !output.status.success(),
        "expected budget regression to fail the evaluator"
    );

    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(output_path).unwrap()).unwrap();
    let performance_gate = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["gate_id"] == "REL-04-performance-budget")
        .expect("performance gate");
    assert_eq!(performance_gate["status"], "failed");
    assert_eq!(performance_gate["reason_code"], "performance_budget_failed");
    assert!(
        performance_gate["detail"]["action"]
            .as_str()
            .unwrap_or_default()
            .contains("Reduce cost"),
        "expected actionable diagnostic in {:?}",
        performance_gate
    );
}
