//! E5.F1.T1: R0-R4 rollout gate automation and acceptance evidence aggregation.
//!
//! Tests the stage-gated rollout governance model for FrankenSqlite promotion,
//! including gate criteria, evidence collection, pass/fail evaluation, and
//! JSON evidence output format.

use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════════
// Rollout stage model
// ═══════════════════════════════════════════════════════════════════════

/// Rollout stages R0-R4.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum RolloutStage {
    R0, // Baseline Hardening
    R1, // Shadow Validation
    R2, // Canary Cutover
    R3, // Progressive Rollout
    R4, // Default Backend Promotion
}

impl RolloutStage {
    fn all() -> Vec<RolloutStage> {
        vec![
            RolloutStage::R0,
            RolloutStage::R1,
            RolloutStage::R2,
            RolloutStage::R3,
            RolloutStage::R4,
        ]
    }

    fn description(&self) -> &str {
        match self {
            RolloutStage::R0 => "Baseline Hardening: prove observability on AppendLog",
            RolloutStage::R1 => "Shadow Validation: migration dry-run without activation",
            RolloutStage::R2 => "Canary Cutover: full M0-M5 in single workspace",
            RolloutStage::R3 => "Progressive Rollout: expand to broader workspace set",
            RolloutStage::R4 => "Default Backend Promotion: FrankenSqlite as preferred",
        }
    }

    fn predecessor(self) -> Option<RolloutStage> {
        match self {
            RolloutStage::R0 => None,
            RolloutStage::R1 => Some(RolloutStage::R0),
            RolloutStage::R2 => Some(RolloutStage::R1),
            RolloutStage::R3 => Some(RolloutStage::R2),
            RolloutStage::R4 => Some(RolloutStage::R3),
        }
    }
}

/// A single criterion that must be satisfied for a gate to pass.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateCriterion {
    name: String,
    description: String,
    satisfied: bool,
    evidence_ref: Option<String>,
}

/// An evidence artifact collected during gate evaluation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EvidenceArtifact {
    artifact_type: String, // "test_result", "metric", "approval", "drill_result"
    name: String,
    path: String,
    collected_at: String,
    valid: bool,
}

/// Soak window metrics for R2/R3 evaluation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SoakMetrics {
    health_checks_passed: u32,
    health_checks_total: u32,
    lag_p99_ms: f64,
    lag_budget_ms: f64,
    invariant_violations: u32,
    soak_duration_hours: f64,
    required_soak_hours: f64,
}

impl SoakMetrics {
    fn health_pass_rate(&self) -> f64 {
        if self.health_checks_total == 0 {
            return 0.0;
        }
        self.health_checks_passed as f64 / self.health_checks_total as f64
    }

    fn lag_within_budget(&self) -> bool {
        self.lag_p99_ms <= self.lag_budget_ms
    }

    fn soak_complete(&self) -> bool {
        self.soak_duration_hours >= self.required_soak_hours
    }
}

/// A complete rollout gate evaluation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RolloutGate {
    stage: RolloutStage,
    timestamp: String,
    commit_sha: String,
    criteria: Vec<GateCriterion>,
    evidence: Vec<EvidenceArtifact>,
    soak_metrics: Option<SoakMetrics>,
    passed: bool,
    notes: Vec<String>,
}

/// Gate evidence output package.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateEvidencePackage {
    schema_version: String,
    gate: RolloutGate,
}

const GATE_EVIDENCE_SCHEMA: &str = "ft.gate-evidence.v1";

// ═══════════════════════════════════════════════════════════════════════
// Gate evaluation logic
// ═══════════════════════════════════════════════════════════════════════

/// Evaluate R0 gate: all T1-T6 green + rollback drill template exists.
fn evaluate_r0(tier_results: &HashMap<String, bool>, rollback_drill_exists: bool) -> RolloutGate {
    let mut criteria = Vec::new();

    // T1-T5 must be green (blocking)
    for tier in &["T1", "T2", "T3", "T4", "T5"] {
        let ok = tier_results.get(*tier).copied().unwrap_or(false);
        criteria.push(GateCriterion {
            name: format!("{tier} green"),
            description: format!("{tier} blocking tier passes"),
            satisfied: ok,
            evidence_ref: Some(format!("tier_result_{tier}.json")),
        });
    }

    // T6 advisory (warn if failed but don't block)
    let t6_ok = tier_results.get("T6").copied().unwrap_or(false);
    criteria.push(GateCriterion {
        name: "T6 advisory".to_string(),
        description: "T6 performance tier (advisory, non-blocking)".to_string(),
        satisfied: t6_ok,
        evidence_ref: Some("tier_result_T6.json".to_string()),
    });

    // Rollback drill
    criteria.push(GateCriterion {
        name: "Rollback drill template".to_string(),
        description: "Rollback drill template exists and is validated".to_string(),
        satisfied: rollback_drill_exists,
        evidence_ref: Some("rollback_drill_template.md".to_string()),
    });

    let blocking_ok = criteria
        .iter()
        .filter(|c| c.name != "T6 advisory")
        .all(|c| c.satisfied);

    RolloutGate {
        stage: RolloutStage::R0,
        timestamp: "2026-02-22T11:00:00Z".to_string(),
        commit_sha: "abc123".to_string(),
        criteria,
        evidence: vec![],
        soak_metrics: None,
        passed: blocking_ok,
        notes: vec![],
    }
}

/// Evaluate R1 gate: shadow migration ran without invariant violations.
fn evaluate_r1(shadow_ran: bool, invariant_violations: u32) -> RolloutGate {
    let criteria = vec![
        GateCriterion {
            name: "Shadow migration executed".to_string(),
            description: "M0-M4 ran in shadow mode (no M5 activation)".to_string(),
            satisfied: shadow_ran,
            evidence_ref: Some("shadow_migration_log.json".to_string()),
        },
        GateCriterion {
            name: "Zero invariant violations".to_string(),
            description: "No digest, cardinality, or ordering violations during shadow".to_string(),
            satisfied: invariant_violations == 0,
            evidence_ref: Some("invariant_check_report.json".to_string()),
        },
    ];

    let passed = criteria.iter().all(|c| c.satisfied);

    RolloutGate {
        stage: RolloutStage::R1,
        timestamp: "2026-02-22T12:00:00Z".to_string(),
        commit_sha: "def456".to_string(),
        criteria,
        evidence: vec![],
        soak_metrics: None,
        passed,
        notes: vec![],
    }
}

/// Evaluate R2 gate: canary soak window passes health/lag/correctness.
fn evaluate_r2(soak: &SoakMetrics, rollback_runbook_approved: bool) -> RolloutGate {
    let criteria = vec![
        GateCriterion {
            name: "Health pass rate >= 99%".to_string(),
            description: "Health checks pass at >= 99% during soak window".to_string(),
            satisfied: soak.health_pass_rate() >= 0.99,
            evidence_ref: Some("soak_health_metrics.json".to_string()),
        },
        GateCriterion {
            name: "Lag within budget".to_string(),
            description: "p99 lag stays within budget during soak".to_string(),
            satisfied: soak.lag_within_budget(),
            evidence_ref: Some("soak_lag_metrics.json".to_string()),
        },
        GateCriterion {
            name: "Zero invariant violations".to_string(),
            description: "No data integrity violations during canary".to_string(),
            satisfied: soak.invariant_violations == 0,
            evidence_ref: Some("canary_invariant_report.json".to_string()),
        },
        GateCriterion {
            name: "Soak window complete".to_string(),
            description: "Soak ran for required minimum duration".to_string(),
            satisfied: soak.soak_complete(),
            evidence_ref: Some("soak_duration_report.json".to_string()),
        },
        GateCriterion {
            name: "Rollback runbook approved".to_string(),
            description: "Rollback runbook reviewed and approved by operator".to_string(),
            satisfied: rollback_runbook_approved,
            evidence_ref: Some("rollback_runbook_approval.json".to_string()),
        },
    ];

    let passed = criteria.iter().all(|c| c.satisfied);

    RolloutGate {
        stage: RolloutStage::R2,
        timestamp: "2026-02-22T18:00:00Z".to_string(),
        commit_sha: "ghi789".to_string(),
        criteria,
        evidence: vec![],
        soak_metrics: Some(soak.clone()),
        passed,
        notes: vec![],
    }
}

/// Evaluate R3 gate: multi-workspace SLO compliance.
fn evaluate_r3(workspaces_passed: u32, workspaces_total: u32, incidents: u32) -> RolloutGate {
    let compliance_rate = if workspaces_total > 0 {
        workspaces_passed as f64 / workspaces_total as f64
    } else {
        0.0
    };

    let mut criteria = Vec::new();

    criteria.push(GateCriterion {
        name: "Workspace compliance >= 95%".to_string(),
        description: format!("{workspaces_passed}/{workspaces_total} workspaces SLO-compliant"),
        satisfied: compliance_rate >= 0.95,
        evidence_ref: Some("workspace_compliance_report.json".to_string()),
    });

    criteria.push(GateCriterion {
        name: "Zero high-severity incidents".to_string(),
        description: "No Tier 2/3 rollback events during progressive rollout".to_string(),
        satisfied: incidents == 0,
        evidence_ref: Some("incident_log.json".to_string()),
    });

    let passed = criteria.iter().all(|c| c.satisfied);

    RolloutGate {
        stage: RolloutStage::R3,
        timestamp: "2026-02-23T12:00:00Z".to_string(),
        commit_sha: "jkl012".to_string(),
        criteria,
        evidence: vec![],
        soak_metrics: None,
        passed,
        notes: vec![],
    }
}

/// Evaluate R4 gate: promotion criteria.
fn evaluate_r4(r3_passed: bool, all_regression_tests_green: bool) -> RolloutGate {
    let criteria = vec![
        GateCriterion {
            name: "R3 gate passed".to_string(),
            description: "Progressive rollout completed successfully".to_string(),
            satisfied: r3_passed,
            evidence_ref: Some("r3_gate_evidence.json".to_string()),
        },
        GateCriterion {
            name: "Full regression suite green".to_string(),
            description: "All T1-T6 tiers pass on promoted backend".to_string(),
            satisfied: all_regression_tests_green,
            evidence_ref: Some("regression_suite_report.json".to_string()),
        },
    ];

    let passed = criteria.iter().all(|c| c.satisfied);

    RolloutGate {
        stage: RolloutStage::R4,
        timestamp: "2026-02-24T08:00:00Z".to_string(),
        commit_sha: "mno345".to_string(),
        criteria,
        evidence: vec![],
        soak_metrics: None,
        passed,
        notes: vec![],
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Rollout stage structure
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_rollout_stages_count() {
    assert_eq!(RolloutStage::all().len(), 5);
}

#[test]
fn test_rollout_stages_ordered() {
    let stages = RolloutStage::all();
    assert_eq!(stages[0], RolloutStage::R0);
    assert_eq!(stages[4], RolloutStage::R4);
}

#[test]
fn test_rollout_stages_all_have_descriptions() {
    for stage in RolloutStage::all() {
        assert!(!stage.description().is_empty());
    }
}

#[test]
fn test_rollout_r0_no_predecessor() {
    assert_eq!(RolloutStage::R0.predecessor(), None);
}

#[test]
fn test_rollout_stage_chain() {
    assert_eq!(RolloutStage::R1.predecessor(), Some(RolloutStage::R0));
    assert_eq!(RolloutStage::R2.predecessor(), Some(RolloutStage::R1));
    assert_eq!(RolloutStage::R3.predecessor(), Some(RolloutStage::R2));
    assert_eq!(RolloutStage::R4.predecessor(), Some(RolloutStage::R3));
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: R0 gate
// ═══════════════════════════════════════════════════════════════════════

fn all_tiers_green() -> HashMap<String, bool> {
    [
        ("T1", true),
        ("T2", true),
        ("T3", true),
        ("T4", true),
        ("T5", true),
        ("T6", true),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[test]
fn test_r0_gate_passes_with_all_tests_green() {
    let gate = evaluate_r0(&all_tiers_green(), true);
    assert!(gate.passed);
    assert_eq!(gate.stage, RolloutStage::R0);
}

#[test]
fn test_r0_gate_fails_with_test_tier_failure() {
    let mut tiers = all_tiers_green();
    tiers.insert("T2".to_string(), false);
    let gate = evaluate_r0(&tiers, true);
    assert!(!gate.passed);
}

#[test]
fn test_r0_gate_passes_even_with_t6_advisory_fail() {
    let mut tiers = all_tiers_green();
    tiers.insert("T6".to_string(), false);
    let gate = evaluate_r0(&tiers, true);
    assert!(gate.passed, "T6 is advisory and should not block R0");
}

#[test]
fn test_r0_gate_fails_without_rollback_drill() {
    let gate = evaluate_r0(&all_tiers_green(), false);
    assert!(!gate.passed);
}

#[test]
fn test_r0_gate_criteria_count() {
    let gate = evaluate_r0(&all_tiers_green(), true);
    // T1-T6 (6) + rollback drill (1) = 7
    assert_eq!(gate.criteria.len(), 7);
}

#[test]
fn test_r0_gate_evidence_refs_present() {
    let gate = evaluate_r0(&all_tiers_green(), true);
    for criterion in &gate.criteria {
        assert!(criterion.evidence_ref.is_some());
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: R1 gate
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_r1_gate_passes_with_clean_shadow_migration() {
    let gate = evaluate_r1(true, 0);
    assert!(gate.passed);
}

#[test]
fn test_r1_gate_fails_if_shadow_not_run() {
    let gate = evaluate_r1(false, 0);
    assert!(!gate.passed);
}

#[test]
fn test_r1_gate_fails_with_invariant_violations() {
    let gate = evaluate_r1(true, 3);
    assert!(!gate.passed);
}

#[test]
fn test_r1_gate_criteria_count() {
    let gate = evaluate_r1(true, 0);
    assert_eq!(gate.criteria.len(), 2);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: R2 gate
// ═══════════════════════════════════════════════════════════════════════

fn healthy_soak() -> SoakMetrics {
    SoakMetrics {
        health_checks_passed: 1000,
        health_checks_total: 1000,
        lag_p99_ms: 25.0,
        lag_budget_ms: 50.0,
        invariant_violations: 0,
        soak_duration_hours: 24.0,
        required_soak_hours: 12.0,
    }
}

#[test]
fn test_r2_gate_requires_soak_window_metrics() {
    let gate = evaluate_r2(&healthy_soak(), true);
    assert!(gate.passed);
    assert!(gate.soak_metrics.is_some());
}

#[test]
fn test_r2_gate_fails_with_poor_health() {
    let mut soak = healthy_soak();
    soak.health_checks_passed = 950; // 95% < 99%
    let gate = evaluate_r2(&soak, true);
    assert!(!gate.passed);
}

#[test]
fn test_r2_gate_fails_with_lag_over_budget() {
    let mut soak = healthy_soak();
    soak.lag_p99_ms = 75.0; // > 50ms budget
    let gate = evaluate_r2(&soak, true);
    assert!(!gate.passed);
}

#[test]
fn test_r2_gate_fails_with_invariant_violations() {
    let mut soak = healthy_soak();
    soak.invariant_violations = 1;
    let gate = evaluate_r2(&soak, true);
    assert!(!gate.passed);
}

#[test]
fn test_r2_gate_fails_without_soak_completion() {
    let mut soak = healthy_soak();
    soak.soak_duration_hours = 6.0; // < 12h required
    let gate = evaluate_r2(&soak, true);
    assert!(!gate.passed);
}

#[test]
fn test_r2_gate_fails_without_runbook_approval() {
    let gate = evaluate_r2(&healthy_soak(), false);
    assert!(!gate.passed);
}

#[test]
fn test_r2_gate_criteria_count() {
    let gate = evaluate_r2(&healthy_soak(), true);
    assert_eq!(gate.criteria.len(), 5);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: R3 gate
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_r3_gate_passes_with_full_compliance() {
    let gate = evaluate_r3(20, 20, 0);
    assert!(gate.passed);
}

#[test]
fn test_r3_gate_fails_below_95_percent() {
    let gate = evaluate_r3(18, 20, 0); // 90% < 95%
    assert!(!gate.passed);
}

#[test]
fn test_r3_gate_fails_with_incidents() {
    let gate = evaluate_r3(20, 20, 1);
    assert!(!gate.passed);
}

#[test]
fn test_r3_gate_passes_at_95_percent() {
    let gate = evaluate_r3(19, 20, 0); // 95% exactly
    assert!(gate.passed);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: R4 gate
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_r4_gate_passes_with_all_criteria() {
    let gate = evaluate_r4(true, true);
    assert!(gate.passed);
}

#[test]
fn test_r4_gate_fails_without_r3() {
    let gate = evaluate_r4(false, true);
    assert!(!gate.passed);
}

#[test]
fn test_r4_gate_fails_without_regression_suite() {
    let gate = evaluate_r4(true, false);
    assert!(!gate.passed);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Evidence output format
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_gate_evidence_output_format() {
    let gate = evaluate_r0(&all_tiers_green(), true);
    let package = GateEvidencePackage {
        schema_version: GATE_EVIDENCE_SCHEMA.to_string(),
        gate,
    };
    let json = serde_json::to_string_pretty(&package).unwrap();
    let reparsed: GateEvidencePackage = serde_json::from_str(&json).unwrap();
    assert_eq!(reparsed.schema_version, GATE_EVIDENCE_SCHEMA);
    assert!(reparsed.gate.passed);
}

#[test]
fn test_gate_evidence_schema_version() {
    assert_eq!(GATE_EVIDENCE_SCHEMA, "ft.gate-evidence.v1");
}

#[test]
fn test_gate_evidence_has_commit_sha() {
    let gate = evaluate_r0(&all_tiers_green(), true);
    assert!(!gate.commit_sha.is_empty());
}

#[test]
fn test_gate_evidence_has_timestamp() {
    let gate = evaluate_r0(&all_tiers_green(), true);
    assert!(!gate.timestamp.is_empty());
}

#[test]
fn test_gate_evidence_json_has_all_fields() {
    let gate = evaluate_r2(&healthy_soak(), true);
    let package = GateEvidencePackage {
        schema_version: GATE_EVIDENCE_SCHEMA.to_string(),
        gate,
    };
    let json = serde_json::to_string(&package).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let gate_obj = value.get("gate").unwrap().as_object().unwrap();
    assert!(gate_obj.contains_key("stage"));
    assert!(gate_obj.contains_key("criteria"));
    assert!(gate_obj.contains_key("passed"));
    assert!(gate_obj.contains_key("soak_metrics"));
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Soak metrics
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_soak_health_pass_rate_100() {
    let soak = healthy_soak();
    assert!((soak.health_pass_rate() - 1.0).abs() < 0.001);
}

#[test]
fn test_soak_health_pass_rate_zero_total() {
    let soak = SoakMetrics {
        health_checks_passed: 0,
        health_checks_total: 0,
        lag_p99_ms: 0.0,
        lag_budget_ms: 50.0,
        invariant_violations: 0,
        soak_duration_hours: 0.0,
        required_soak_hours: 12.0,
    };
    assert!((soak.health_pass_rate()).abs() < 0.001);
}

#[test]
fn test_soak_lag_within_budget() {
    let soak = healthy_soak();
    assert!(soak.lag_within_budget());
}

#[test]
fn test_soak_lag_over_budget() {
    let mut soak = healthy_soak();
    soak.lag_p99_ms = 51.0;
    assert!(!soak.lag_within_budget());
}

#[test]
fn test_soak_complete() {
    let soak = healthy_soak();
    assert!(soak.soak_complete());
}

#[test]
fn test_soak_incomplete() {
    let mut soak = healthy_soak();
    soak.soak_duration_hours = 6.0;
    assert!(!soak.soak_complete());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Gate stage serialization
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_rollout_stage_serde_roundtrip() {
    for stage in RolloutStage::all() {
        let json = serde_json::to_string(&stage).unwrap();
        let back: RolloutStage = serde_json::from_str(&json).unwrap();
        assert_eq!(stage, back);
    }
}

#[test]
fn test_rollout_stage_serializes_uppercase() {
    let json = serde_json::to_string(&RolloutStage::R0).unwrap();
    assert_eq!(json, "\"R0\"");
}
