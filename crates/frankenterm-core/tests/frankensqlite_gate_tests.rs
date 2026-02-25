//! E4.F2.T3: CI quality gate definitions and validation tests.
//!
//! Validates that the gate tier structure, blocking rules, timeout budgets,
//! and wave readiness (BT) preconditions are correctly defined and consistent.

use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════════
// Gate tier model
// ═══════════════════════════════════════════════════════════════════════

/// A test tier in the CI quality gate system.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum GateTier {
    T1, // Unit / Contract
    T2, // Integration / E2E
    T3, // Rollback scenarios
    T4, // Logging assertions
    T5, // Fixture validation
    T6, // Performance / SLO (advisory)
}

impl GateTier {
    fn all() -> Vec<GateTier> {
        vec![
            GateTier::T1,
            GateTier::T2,
            GateTier::T3,
            GateTier::T4,
            GateTier::T5,
            GateTier::T6,
        ]
    }

    fn is_blocking(&self) -> bool {
        !matches!(self, GateTier::T6)
    }

    fn timeout_seconds(&self) -> u64 {
        match self {
            GateTier::T1 => 60,
            GateTier::T2 => 120,
            GateTier::T3 => 120,
            GateTier::T4 => 60,
            GateTier::T5 => 60,
            GateTier::T6 => 300,
        }
    }

    fn test_binary(&self) -> &str {
        match self {
            GateTier::T1 => "frankensqlite_contract_tests",
            GateTier::T2 => "frankensqlite_e2e_tests",
            GateTier::T3 => "frankensqlite_e2e_tests",
            GateTier::T4 => "frankensqlite_logging_tests",
            GateTier::T5 => "frankensqlite_fixture_tests",
            GateTier::T6 => "frankensqlite_perf_tests",
        }
    }

    fn description(&self) -> &str {
        match self {
            GateTier::T1 => "Contract tests: RecorderStorage seam invariants",
            GateTier::T2 => "E2E migration: M0-M5 pipeline + cutover",
            GateTier::T3 => "Rollback scenarios: failure injection + tier classification",
            GateTier::T4 => "Logging assertions: structured field presence + level correctness",
            GateTier::T5 => "Fixture validation: load, checksum, schema version, roundtrip",
            GateTier::T6 => "Performance SLO gates: throughput + latency budgets (advisory)",
        }
    }

    fn label(&self) -> &str {
        match self {
            GateTier::T1 => "T1",
            GateTier::T2 => "T2",
            GateTier::T3 => "T3",
            GateTier::T4 => "T4",
            GateTier::T5 => "T5",
            GateTier::T6 => "T6",
        }
    }
}

/// Bead tier (wave readiness) in the rollout governance system.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BeadTier {
    BT1, // W0: Seam Hardening
    BT2, // W1: Backend Implementation
    BT3, // W2: Verification
    BT4, // W3: Rollout
    BT5, // W4: Cleanup
}

impl BeadTier {
    fn all() -> Vec<BeadTier> {
        vec![
            BeadTier::BT1,
            BeadTier::BT2,
            BeadTier::BT3,
            BeadTier::BT4,
            BeadTier::BT5,
        ]
    }

    fn required_tiers(&self) -> Vec<GateTier> {
        match self {
            BeadTier::BT1 => vec![GateTier::T1],
            BeadTier::BT2 => vec![GateTier::T1, GateTier::T2],
            BeadTier::BT3 => vec![
                GateTier::T1,
                GateTier::T2,
                GateTier::T3,
                GateTier::T4,
                GateTier::T5,
            ],
            BeadTier::BT4 => GateTier::all(), // Including advisory T6
            BeadTier::BT5 => GateTier::all(),
        }
    }

    fn description(&self) -> &str {
        match self {
            BeadTier::BT1 => "W0 Seam Hardening: contract invariants verified",
            BeadTier::BT2 => "W1 Backend Implementation: migration pipeline functional",
            BeadTier::BT3 => "W2 Verification: all blocking gates green",
            BeadTier::BT4 => "W3 Rollout: performance within budget, ready for deploy",
            BeadTier::BT5 => "W4 Cleanup: no regressions, all gates green",
        }
    }

    fn label(&self) -> &str {
        match self {
            BeadTier::BT1 => "BT1",
            BeadTier::BT2 => "BT2",
            BeadTier::BT3 => "BT3",
            BeadTier::BT4 => "BT4",
            BeadTier::BT5 => "BT5",
        }
    }
}

/// Simulated gate execution result.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct GateResult {
    tier: GateTier,
    passed: bool,
    duration_s: u64,
}

/// Evaluate wave readiness given a set of gate results.
fn evaluate_wave_readiness(results: &[GateResult]) -> HashMap<BeadTier, bool> {
    let pass_set: std::collections::HashSet<&GateTier> = results
        .iter()
        .filter(|r| r.passed)
        .map(|r| &r.tier)
        .collect();

    BeadTier::all()
        .into_iter()
        .map(|bt| {
            let ready = bt.required_tiers().iter().all(|t| pass_set.contains(t));
            (bt, ready)
        })
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Gate tier structure
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_gate_tiers_are_exactly_six() {
    assert_eq!(GateTier::all().len(), 6);
}

#[test]
fn test_gate_t1_through_t5_are_blocking() {
    for tier in &[
        GateTier::T1,
        GateTier::T2,
        GateTier::T3,
        GateTier::T4,
        GateTier::T5,
    ] {
        assert!(tier.is_blocking(), "{:?} should be blocking", tier);
    }
}

#[test]
fn test_gate_t6_is_advisory() {
    assert!(!GateTier::T6.is_blocking());
}

#[test]
fn test_gate_timeouts_within_ci_budget() {
    let total: u64 = GateTier::all().iter().map(|t| t.timeout_seconds()).sum();
    // Total timeout budget should fit within 30-minute CI job
    assert!(total <= 1800, "total gate timeout {total}s exceeds 30m");
}

#[test]
fn test_gate_t1_timeout_60s() {
    assert_eq!(GateTier::T1.timeout_seconds(), 60);
}

#[test]
fn test_gate_t2_timeout_120s() {
    assert_eq!(GateTier::T2.timeout_seconds(), 120);
}

#[test]
fn test_gate_t3_timeout_120s() {
    assert_eq!(GateTier::T3.timeout_seconds(), 120);
}

#[test]
fn test_gate_t4_timeout_60s() {
    assert_eq!(GateTier::T4.timeout_seconds(), 60);
}

#[test]
fn test_gate_t5_timeout_60s() {
    assert_eq!(GateTier::T5.timeout_seconds(), 60);
}

#[test]
fn test_gate_t6_timeout_300s() {
    assert_eq!(GateTier::T6.timeout_seconds(), 300);
}

#[test]
fn test_gate_each_tier_has_unique_label() {
    let tiers = GateTier::all();
    let labels: Vec<&str> = tiers.iter().map(|t| t.label()).collect();
    let unique: std::collections::HashSet<&&str> = labels.iter().collect();
    assert_eq!(labels.len(), unique.len());
}

#[test]
fn test_gate_each_tier_has_nonempty_description() {
    for tier in GateTier::all() {
        assert!(
            !tier.description().is_empty(),
            "{:?} has empty description",
            tier
        );
    }
}

#[test]
fn test_gate_each_tier_has_test_binary() {
    for tier in GateTier::all() {
        assert!(
            tier.test_binary().starts_with("frankensqlite_"),
            "{:?} test binary should start with frankensqlite_",
            tier
        );
    }
}

#[test]
fn test_gate_t3_shares_binary_with_t2() {
    // T3 (rollback) filters within the same binary as T2 (e2e)
    assert_eq!(GateTier::T2.test_binary(), GateTier::T3.test_binary());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Bead tier wave readiness
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_bead_tiers_are_exactly_five() {
    assert_eq!(BeadTier::all().len(), 5);
}

#[test]
fn test_bt1_requires_only_t1() {
    assert_eq!(BeadTier::BT1.required_tiers(), vec![GateTier::T1]);
}

#[test]
fn test_bt2_requires_t1_and_t2() {
    assert_eq!(
        BeadTier::BT2.required_tiers(),
        vec![GateTier::T1, GateTier::T2]
    );
}

#[test]
fn test_bt3_requires_t1_through_t5() {
    let required = BeadTier::BT3.required_tiers();
    assert_eq!(required.len(), 5);
    assert!(!required.contains(&GateTier::T6));
}

#[test]
fn test_bt4_requires_all_tiers_including_advisory() {
    let required = BeadTier::BT4.required_tiers();
    assert_eq!(required.len(), 6);
    assert!(required.contains(&GateTier::T6));
}

#[test]
fn test_bt5_requires_all_tiers() {
    assert_eq!(BeadTier::BT5.required_tiers().len(), 6);
}

#[test]
fn test_bead_tier_labels_unique() {
    let tiers = BeadTier::all();
    let labels: Vec<&str> = tiers.iter().map(|t| t.label()).collect();
    let unique: std::collections::HashSet<&&str> = labels.iter().collect();
    assert_eq!(labels.len(), unique.len());
}

#[test]
fn test_bead_tier_descriptions_nonempty() {
    for bt in BeadTier::all() {
        assert!(
            !bt.description().is_empty(),
            "{:?} has empty description",
            bt
        );
    }
}

#[test]
fn test_bead_tier_monotonic_requirements() {
    // Each tier should require at least as many gates as the previous
    let tiers = BeadTier::all();
    for window in tiers.windows(2) {
        let prev_count = window[0].required_tiers().len();
        let next_count = window[1].required_tiers().len();
        assert!(
            next_count >= prev_count,
            "{:?} requires {} gates but {:?} requires {} — not monotonic",
            window[1],
            next_count,
            window[0],
            prev_count,
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Wave readiness evaluation
// ═══════════════════════════════════════════════════════════════════════

fn all_pass() -> Vec<GateResult> {
    GateTier::all()
        .into_iter()
        .map(|t| GateResult {
            tier: t,
            passed: true,
            duration_s: 10,
        })
        .collect()
}

fn all_fail() -> Vec<GateResult> {
    GateTier::all()
        .into_iter()
        .map(|t| GateResult {
            tier: t,
            passed: false,
            duration_s: 1,
        })
        .collect()
}

#[test]
fn test_wave_all_pass_all_tiers_ready() {
    let readiness = evaluate_wave_readiness(&all_pass());
    for bt in BeadTier::all() {
        assert!(readiness[&bt], "{:?} should be ready when all pass", bt);
    }
}

#[test]
fn test_wave_all_fail_no_tiers_ready() {
    let readiness = evaluate_wave_readiness(&all_fail());
    for bt in BeadTier::all() {
        assert!(
            !readiness[&bt],
            "{:?} should not be ready when all fail",
            bt
        );
    }
}

#[test]
fn test_wave_only_t1_passes_bt1_ready() {
    let results = vec![GateResult {
        tier: GateTier::T1,
        passed: true,
        duration_s: 5,
    }];
    let readiness = evaluate_wave_readiness(&results);
    assert!(readiness[&BeadTier::BT1]);
    assert!(!readiness[&BeadTier::BT2]);
}

#[test]
fn test_wave_t1_t2_pass_bt2_ready() {
    let results = vec![
        GateResult {
            tier: GateTier::T1,
            passed: true,
            duration_s: 5,
        },
        GateResult {
            tier: GateTier::T2,
            passed: true,
            duration_s: 10,
        },
    ];
    let readiness = evaluate_wave_readiness(&results);
    assert!(readiness[&BeadTier::BT1]);
    assert!(readiness[&BeadTier::BT2]);
    assert!(!readiness[&BeadTier::BT3]);
}

#[test]
fn test_wave_t1_through_t5_pass_bt3_ready() {
    let results: Vec<_> = [
        GateTier::T1,
        GateTier::T2,
        GateTier::T3,
        GateTier::T4,
        GateTier::T5,
    ]
    .into_iter()
    .map(|t| GateResult {
        tier: t,
        passed: true,
        duration_s: 5,
    })
    .collect();
    let readiness = evaluate_wave_readiness(&results);
    assert!(readiness[&BeadTier::BT3]);
    assert!(!readiness[&BeadTier::BT4]); // T6 missing
}

#[test]
fn test_wave_t6_advisory_fail_blocks_bt4() {
    let mut results = all_pass();
    results.iter_mut().for_each(|r| {
        if r.tier == GateTier::T6 {
            r.passed = false;
        }
    });
    let readiness = evaluate_wave_readiness(&results);
    assert!(readiness[&BeadTier::BT3]);
    assert!(!readiness[&BeadTier::BT4]); // T6 needed for BT4
}

#[test]
fn test_wave_single_blocking_fail_cascades() {
    let mut results = all_pass();
    results.iter_mut().for_each(|r| {
        if r.tier == GateTier::T2 {
            r.passed = false;
        }
    });
    let readiness = evaluate_wave_readiness(&results);
    assert!(readiness[&BeadTier::BT1]); // T1 still OK
    assert!(!readiness[&BeadTier::BT2]); // T2 failed
    assert!(!readiness[&BeadTier::BT3]); // Cascades
}

#[test]
fn test_wave_empty_results_nothing_ready() {
    let readiness = evaluate_wave_readiness(&[]);
    for bt in BeadTier::all() {
        assert!(!readiness[&bt]);
    }
}

#[test]
fn test_wave_readiness_returns_all_bead_tiers() {
    let readiness = evaluate_wave_readiness(&all_pass());
    assert_eq!(readiness.len(), 5);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Gate report structure
// ═══════════════════════════════════════════════════════════════════════

/// Gate report JSON structure.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct GateReport {
    timestamp: String,
    script: String,
    blocking_pass: u32,
    blocking_fail: u32,
    advisory_fail: u32,
    overall: String,
    gates: Vec<GateEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct GateEntry {
    tier: String,
    name: String,
    blocking: bool,
    result: String,
    exit_code: i32,
    duration_s: u64,
}

#[test]
fn test_gate_report_schema_serialization() {
    let json = r#"{
        "timestamp": "2026-02-22T10:00:00Z",
        "script": "ci_frankensqlite_gates.sh",
        "blocking_pass": 5,
        "blocking_fail": 0,
        "advisory_fail": 1,
        "overall": "pass",
        "gates": [
            {"tier": "T1", "name": "Contract tests", "blocking": true, "result": "pass", "exit_code": 0, "duration_s": 15}
        ]
    }"#;
    let report: GateReport = serde_json::from_str(json).unwrap();
    assert_eq!(report.blocking_pass, 5);
    assert_eq!(report.overall, "pass");
    assert_eq!(report.gates.len(), 1);
}

#[test]
fn test_gate_report_fail_overall() {
    let json = r#"{
        "timestamp": "2026-02-22T10:00:00Z",
        "script": "ci_frankensqlite_gates.sh",
        "blocking_pass": 4,
        "blocking_fail": 1,
        "advisory_fail": 0,
        "overall": "fail",
        "gates": []
    }"#;
    let report: GateReport = serde_json::from_str(json).unwrap();
    assert_eq!(report.overall, "fail");
    assert_eq!(report.blocking_fail, 1);
}

#[test]
fn test_gate_entry_result_values() {
    // Valid result values: pass, fail, advisory_fail
    for result in &["pass", "fail", "advisory_fail"] {
        let json = format!(
            r#"{{"tier": "T1", "name": "test", "blocking": true, "result": "{result}", "exit_code": 0, "duration_s": 5}}"#
        );
        let entry: GateEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.result, *result);
    }
}

#[test]
fn test_gate_entry_blocking_flag_matches_tier() {
    // T1-T5 should be blocking, T6 advisory
    let expectations: Vec<(&str, bool)> = vec![
        ("T1", true),
        ("T2", true),
        ("T3", true),
        ("T4", true),
        ("T5", true),
        ("T6", false),
    ];
    for (tier_label, expected_blocking) in expectations {
        let tier = GateTier::all()
            .into_iter()
            .find(|t| t.label() == tier_label)
            .unwrap();
        assert_eq!(
            tier.is_blocking(),
            expected_blocking,
            "{tier_label} blocking mismatch"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: CI script existence (file-system validation)
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_ci_gate_script_exists() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = std::path::PathBuf::from(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("scripts")
        .join("ci_frankensqlite_gates.sh");
    assert!(
        script.exists(),
        "CI gate script missing at {}",
        script.display()
    );
}

#[test]
fn test_ci_gate_script_executable() {
    use std::os::unix::fs::PermissionsExt;
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let script = std::path::PathBuf::from(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("scripts")
        .join("ci_frankensqlite_gates.sh");
    let metadata = std::fs::metadata(&script).unwrap();
    let mode = metadata.permissions().mode();
    assert!(mode & 0o111 != 0, "CI gate script should be executable");
}

#[test]
fn test_operator_journey_scripts_exist() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let base = std::path::PathBuf::from(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("scripts");
    assert!(base.join("operator_migration_journey.sh").exists());
    assert!(base.join("operator_incident_triage.sh").exists());
}

#[test]
fn test_tier_test_scripts_exist() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let base = std::path::PathBuf::from(&manifest)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("scripts");
    let scripts = [
        "test_frankensqlite_basic.sh",
        "test_frankensqlite_migration.sh",
        "test_frankensqlite_rollback.sh",
        "test_frankensqlite_soak.sh",
    ];
    for name in &scripts {
        assert!(base.join(name).exists(), "Missing test script: {name}");
    }
}
