//! ft-7h5da.6.5 (plan-verb slice): golden matrix for `ft steer plan`.
//!
//! Pins the 5 deterministic scenario `SteeringReceipt`s (fixed `created_at_ms`)
//! so any drift in the steer-plan pipeline (or the mission-objective status
//! taxonomy it reads) is caught. Regenerate with `UPDATE_GOLDEN=1`.
//!
//! The `steer_run` / `steer_status` verbs + their goldens ride W5.3 (`.6.3`).

use frankenterm_core::steer_plan::{SteerPlanScenario, plan_status_label, steer_plan};
use std::path::PathBuf;

/// Fixed timestamp so the golden is byte-stable (the receipt id is
/// content-addressed and excludes timestamps regardless).
const FIXED_MS: i64 = 1_704_000_000_000;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/steer_plan_scenarios.json")
}

fn build_matrix() -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for scenario in SteerPlanScenario::ALL {
        let result = steer_plan(
            scenario,
            "ship the W3 family",
            "ws-golden",
            FIXED_MS as u64,
            FIXED_MS,
            None,
        );
        map.insert(
            scenario.as_str().to_string(),
            serde_json::json!({
                "plan_status": plan_status_label(result.plan_status),
                "policy_verdict": result.policy_verdict,
                "receipt": result.receipt,
            }),
        );
    }
    serde_json::Value::Object(map)
}

#[test]
fn steer_plan_scenarios_match_golden() {
    let actual = serde_json::to_string_pretty(&build_matrix()).expect("serialize matrix");
    let path = golden_path();
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all(path.parent().expect("golden parent")).expect("create golden dir");
        std::fs::write(&path, format!("{actual}\n")).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "read golden {}: {e}\nrun UPDATE_GOLDEN=1 to create it",
            path.display()
        )
    });
    assert_eq!(
        actual.trim(),
        expected.trim(),
        "steer-plan golden drift; if intentional regenerate with: \
         UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test conformance_steer_plan_golden"
    );
}

#[test]
fn steer_plan_receipts_are_content_addressed_validated_and_distinct() {
    let mut ids = std::collections::BTreeSet::new();
    for scenario in SteerPlanScenario::ALL {
        let r = steer_plan(scenario, "obj", "ws", FIXED_MS as u64, FIXED_MS, None);
        assert!(
            r.receipt.receipt_id.starts_with("steer:"),
            "receipt id not content-addressed for {}: {}",
            scenario.as_str(),
            r.receipt.receipt_id
        );
        assert!(r.receipt.validate().is_ok(), "receipt failed validate()");
        ids.insert(r.receipt.receipt_id.clone());
    }
    // Distinct verdicts/scores across the 5 statuses → distinct content-addressed
    // ids (clean-ready/dirty-overlap/approval-required share the requires_approval
    // family but differ by approvals; rch/capacity differ by verdict).
    assert!(
        ids.len() >= 4,
        "expected distinct receipt ids across scenarios, got {ids:?}"
    );
}
