//! Static verification catalog for latency-stage invariants.

use super::LatencyStage;
use serde::{Deserialize, Serialize};

/// Test scenario category for the verification matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TestCategory {
    /// Unit tests for individual functions.
    Unit,
    /// Property-based tests (proptest/quickcheck).
    Property,
    /// Integration tests across module boundaries.
    Integration,
    /// End-to-end pipeline tests.
    EndToEnd,
    /// Chaos/fault injection tests.
    Chaos,
    /// Sustained load (soak) tests.
    Soak,
}

/// A single entry in the verification matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationEntry {
    /// Test scenario name.
    pub name: String,
    /// Which category this test belongs to.
    pub category: TestCategory,
    /// Which stage(s) this test covers.
    pub stages: Vec<LatencyStage>,
    /// Conditions: nominal, degraded, failure, recovery, etc.
    pub conditions: Vec<String>,
    /// Expected invariants that must hold.
    pub invariants: Vec<String>,
    /// Minimum sample count for statistical significance.
    pub min_samples: u32,
}

/// The complete verification matrix for the latency stages module.
pub fn verification_matrix() -> Vec<VerificationEntry> {
    vec![
        // Unit tests.
        VerificationEntry {
            name: "stage_budget_construction_valid".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec![
                "non-negative targets".into(),
                "monotonic percentiles".into(),
            ],
            min_samples: 1,
        },
        VerificationEntry {
            name: "stage_budget_rejects_negative".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["error".into()],
            invariants: vec!["NegativeTarget error returned".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "stage_budget_rejects_nonmonotonic".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["error".into()],
            invariants: vec!["NonMonotonic error returned".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "budget_tree_sequential_composition".into(),
            category: TestCategory::Unit,
            stages: LatencyStage::CAPTURE_PATH.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec!["aggregate equals sum of leaves".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "budget_tree_parallel_composition".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["aggregate equals max of branches".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "budget_tree_conditional_composition".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["aggregate equals weighted sum".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "slack_conservation".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["slack = ceiling - aggregate".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "reason_code_display".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["nominal".into()],
            invariants: vec!["formatted reason matches expected pattern".into()],
            min_samples: 1,
        },
        VerificationEntry {
            name: "pipeline_run_validation_happy".into(),
            category: TestCategory::Unit,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec![
                "stage order correct".into(),
                "timestamps non-decreasing".into(),
                "total matches sum".into(),
                "overflow flag consistent".into(),
            ],
            min_samples: 1,
        },
        VerificationEntry {
            name: "pipeline_run_validation_rejects_misordered".into(),
            category: TestCategory::Unit,
            stages: vec![],
            conditions: vec!["error".into()],
            invariants: vec!["StageOrdering violation".into()],
            min_samples: 1,
        },
        // Property tests.
        VerificationEntry {
            name: "proptest_budget_monotonicity".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec![
                "p50 ≤ p95 ≤ p99 ≤ p999".into(),
                "all targets non-negative".into(),
            ],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_sequential_composition_additive".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["Seq aggregate = sum of leaf targets".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_parallel_composition_max".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["Par aggregate = max of branch targets".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_conditional_weighted".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["Cond aggregate = p*then + (1-p)*else".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_slack_conservation".into(),
            category: TestCategory::Property,
            stages: vec![],
            conditions: vec!["random".into()],
            invariants: vec!["slack = ceiling - aggregate (exact)".into()],
            min_samples: 1000,
        },
        VerificationEntry {
            name: "proptest_pipeline_run_roundtrip".into(),
            category: TestCategory::Property,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["random".into()],
            invariants: vec!["serde roundtrip preserves all fields".into()],
            min_samples: 1000,
        },
        // Integration tests.
        VerificationEntry {
            name: "integration_default_budgets_consistency".into(),
            category: TestCategory::Integration,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec![
                "all stages have budgets".into(),
                "aggregate fits within E2E budget".into(),
            ],
            min_samples: 1,
        },
        VerificationEntry {
            name: "integration_benchmark_contract_coverage".into(),
            category: TestCategory::Integration,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["nominal".into()],
            invariants: vec![
                "every non-aggregate stage has criteria".into(),
                "every workload class covered".into(),
            ],
            min_samples: 1,
        },
        // E2E tests.
        VerificationEntry {
            name: "e2e_capture_path_within_budget".into(),
            category: TestCategory::EndToEnd,
            stages: LatencyStage::CAPTURE_PATH.to_vec(),
            conditions: vec!["light_single".into(), "medium_swarm".into()],
            invariants: vec!["total capture latency within E2E budget at p99".into()],
            min_samples: 100,
        },
        VerificationEntry {
            name: "e2e_action_path_within_budget".into(),
            category: TestCategory::EndToEnd,
            stages: LatencyStage::ACTION_PATH.to_vec(),
            conditions: vec!["light_single".into()],
            invariants: vec!["action completion within E2E budget at p99".into()],
            min_samples: 100,
        },
        // Chaos tests.
        VerificationEntry {
            name: "chaos_storage_stall_overflow_isolated".into(),
            category: TestCategory::Chaos,
            stages: vec![LatencyStage::StorageWrite],
            conditions: vec!["storage_degraded".into()],
            invariants: vec![
                "overflow emitted for StorageWrite".into(),
                "downstream stages unaffected".into(),
                "reason code = OVERFLOW_ISOLATED".into(),
            ],
            min_samples: 10,
        },
        VerificationEntry {
            name: "chaos_pattern_storm_shed".into(),
            category: TestCategory::Chaos,
            stages: vec![LatencyStage::PatternDetection],
            conditions: vec!["pattern_storm".into()],
            invariants: vec![
                "detection latency bounded at p999".into(),
                "low-priority detections shed under pressure".into(),
            ],
            min_samples: 10,
        },
        // Soak tests.
        VerificationEntry {
            name: "soak_24h_budget_drift".into(),
            category: TestCategory::Soak,
            stages: LatencyStage::PIPELINE_STAGES.to_vec(),
            conditions: vec!["large_swarm".into()],
            invariants: vec![
                "no percentile drift > 10% over 24h".into(),
                "no monotonic latency increase trend".into(),
            ],
            min_samples: 10000,
        },
    ]
}
