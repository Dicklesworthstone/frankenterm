//! E5.F1.T4: Gate go/no-go on comprehensive unit/integration/e2e evidence package.
//!
//! Tests the evidence completeness checker: every required artifact must exist
//! and be non-empty before the go/no-go gate can pass. Missing artifacts
//! produce a gap report that blocks the rollout.

use std::collections::{BTreeMap, BTreeSet};

// ═══════════════════════════════════════════════════════════════════════
// Evidence tier model
// ═══════════════════════════════════════════════════════════════════════

/// Each required evidence category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum EvidenceTier {
    T1UnitTests,
    T2IntegrationTests,
    T3E2eScenarios,
    T4LoggingAssertions,
    T5FixtureValidation,
    T6PerfSoak,
    MigrationDryRun,
    RollbackDrill,
}

impl EvidenceTier {
    fn all() -> Vec<EvidenceTier> {
        vec![
            EvidenceTier::T1UnitTests,
            EvidenceTier::T2IntegrationTests,
            EvidenceTier::T3E2eScenarios,
            EvidenceTier::T4LoggingAssertions,
            EvidenceTier::T5FixtureValidation,
            EvidenceTier::T6PerfSoak,
            EvidenceTier::MigrationDryRun,
            EvidenceTier::RollbackDrill,
        ]
    }

    fn label(&self) -> &'static str {
        match self {
            EvidenceTier::T1UnitTests => "T1 Unit Tests",
            EvidenceTier::T2IntegrationTests => "T2 Integration Tests",
            EvidenceTier::T3E2eScenarios => "T3 E2E Scenarios",
            EvidenceTier::T4LoggingAssertions => "T4 Logging Assertions",
            EvidenceTier::T5FixtureValidation => "T5 Fixture Validation",
            EvidenceTier::T6PerfSoak => "T6 Perf/Soak",
            EvidenceTier::MigrationDryRun => "Migration Dry Run",
            EvidenceTier::RollbackDrill => "Rollback Drill",
        }
    }

    /// Whether this tier is blocking (T6 is advisory).
    fn is_blocking(&self) -> bool {
        !matches!(self, EvidenceTier::T6PerfSoak)
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Evidence artifact
// ═══════════════════════════════════════════════════════════════════════

/// A single evidence artifact submitted for a tier.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EvidenceArtifact {
    tier: EvidenceTier,
    artifact_name: String,
    byte_count: u64,
    test_count: u32,
    pass_count: u32,
    fail_count: u32,
}

impl EvidenceArtifact {
    fn is_non_empty(&self) -> bool {
        self.byte_count > 0
    }

    fn is_all_pass(&self) -> bool {
        self.fail_count == 0 && self.test_count > 0
    }

    fn pass_rate(&self) -> f64 {
        if self.test_count == 0 {
            return 0.0;
        }
        self.pass_count as f64 / self.test_count as f64
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Evidence package and checker
// ═══════════════════════════════════════════════════════════════════════

/// The collected evidence package for a go/no-go review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct EvidencePackage {
    commit_sha: String,
    artifacts: Vec<EvidenceArtifact>,
}

/// Gap report listing what's missing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GapReport {
    missing_tiers: Vec<EvidenceTier>,
    empty_artifacts: Vec<String>,
    failing_tiers: Vec<(EvidenceTier, u32)>, // tier + fail count
    blocking_gaps: usize,
    advisory_gaps: usize,
    is_complete: bool,
}

/// Check whether the evidence package is complete for go/no-go.
fn check_evidence_completeness(package: &EvidencePackage) -> GapReport {
    let required = EvidenceTier::all();
    let present_tiers: BTreeSet<EvidenceTier> = package.artifacts.iter().map(|a| a.tier).collect();

    let mut missing_tiers = Vec::new();
    let mut empty_artifacts = Vec::new();
    let mut failing_tiers = Vec::new();

    for tier in &required {
        if !present_tiers.contains(tier) {
            missing_tiers.push(*tier);
        }
    }

    let mut empty_blocking_count = 0usize;
    for artifact in &package.artifacts {
        if !artifact.is_non_empty() {
            empty_artifacts.push(artifact.artifact_name.clone());
            if artifact.tier.is_blocking() {
                empty_blocking_count += 1;
            }
        }
        if artifact.fail_count > 0 && artifact.tier.is_blocking() {
            failing_tiers.push((artifact.tier, artifact.fail_count));
        }
    }

    let blocking_gaps = missing_tiers.iter().filter(|t| t.is_blocking()).count()
        + empty_blocking_count
        + failing_tiers.len();
    let advisory_gaps = missing_tiers.iter().filter(|t| !t.is_blocking()).count();
    let is_complete = blocking_gaps == 0;

    GapReport {
        missing_tiers,
        empty_artifacts,
        failing_tiers,
        blocking_gaps,
        advisory_gaps,
        is_complete,
    }
}

/// Whether the go/no-go gate should open.
fn gate_passes(package: &EvidencePackage) -> bool {
    check_evidence_completeness(package).is_complete
}

// ═══════════════════════════════════════════════════════════════════════
// Evidence builder helper
// ═══════════════════════════════════════════════════════════════════════

struct EvidenceBuilder {
    commit_sha: String,
    artifacts: BTreeMap<EvidenceTier, EvidenceArtifact>,
}

impl EvidenceBuilder {
    fn new(commit_sha: &str) -> Self {
        Self {
            commit_sha: commit_sha.to_string(),
            artifacts: BTreeMap::new(),
        }
    }

    fn add_passing(&mut self, tier: EvidenceTier, tests: u32) -> &mut Self {
        self.artifacts.insert(tier, EvidenceArtifact {
            tier,
            artifact_name: format!("{}_results.json", tier.label().to_lowercase().replace(' ', "_")),
            byte_count: 1024 * tests as u64,
            test_count: tests,
            pass_count: tests,
            fail_count: 0,
        });
        self
    }

    fn add_failing(&mut self, tier: EvidenceTier, total: u32, failed: u32) -> &mut Self {
        self.artifacts.insert(tier, EvidenceArtifact {
            tier,
            artifact_name: format!("{}_results.json", tier.label().to_lowercase().replace(' ', "_")),
            byte_count: 1024 * total as u64,
            test_count: total,
            pass_count: total - failed,
            fail_count: failed,
        });
        self
    }

    fn add_empty(&mut self, tier: EvidenceTier) -> &mut Self {
        self.artifacts.insert(tier, EvidenceArtifact {
            tier,
            artifact_name: format!("{}_results.json", tier.label().to_lowercase().replace(' ', "_")),
            byte_count: 0,
            test_count: 0,
            pass_count: 0,
            fail_count: 0,
        });
        self
    }

    fn build_complete_passing(mut self) -> EvidencePackage {
        for tier in EvidenceTier::all() {
            if !self.artifacts.contains_key(&tier) {
                self.add_passing(tier, 10);
            }
        }
        self.build()
    }

    fn build(self) -> EvidencePackage {
        EvidencePackage {
            commit_sha: self.commit_sha,
            artifacts: self.artifacts.into_values().collect(),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Evidence tier model
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_evidence_tier_all_returns_8() {
    assert_eq!(EvidenceTier::all().len(), 8);
}

#[test]
fn test_evidence_tier_t6_is_advisory() {
    assert!(!EvidenceTier::T6PerfSoak.is_blocking());
}

#[test]
fn test_evidence_tier_t1_through_t5_are_blocking() {
    let blocking: Vec<_> = EvidenceTier::all().into_iter().filter(|t| t.is_blocking()).collect();
    // T1-T5 + MigrationDryRun + RollbackDrill = 7 blocking
    assert_eq!(blocking.len(), 7);
}

#[test]
fn test_evidence_tier_labels_unique() {
    let labels: BTreeSet<&str> = EvidenceTier::all().iter().map(|t| t.label()).collect();
    assert_eq!(labels.len(), 8);
}

#[test]
fn test_evidence_tier_serde_roundtrip() {
    for tier in EvidenceTier::all() {
        let json = serde_json::to_string(&tier).unwrap();
        let back: EvidenceTier = serde_json::from_str(&json).unwrap();
        assert_eq!(tier, back);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Artifact properties
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_artifact_non_empty_with_data() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 100,
        test_count: 5,
        pass_count: 5,
        fail_count: 0,
    };
    assert!(a.is_non_empty());
}

#[test]
fn test_artifact_empty_with_zero_bytes() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 0,
        test_count: 0,
        pass_count: 0,
        fail_count: 0,
    };
    assert!(!a.is_non_empty());
}

#[test]
fn test_artifact_all_pass() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 100,
        test_count: 10,
        pass_count: 10,
        fail_count: 0,
    };
    assert!(a.is_all_pass());
}

#[test]
fn test_artifact_not_all_pass_with_failures() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 100,
        test_count: 10,
        pass_count: 8,
        fail_count: 2,
    };
    assert!(!a.is_all_pass());
}

#[test]
fn test_artifact_pass_rate_full() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 100,
        test_count: 20,
        pass_count: 20,
        fail_count: 0,
    };
    assert!((a.pass_rate() - 1.0).abs() < f64::EPSILON);
}

#[test]
fn test_artifact_pass_rate_partial() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 100,
        test_count: 4,
        pass_count: 3,
        fail_count: 1,
    };
    assert!((a.pass_rate() - 0.75).abs() < f64::EPSILON);
}

#[test]
fn test_artifact_pass_rate_zero_tests() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T1UnitTests,
        artifact_name: "t1.json".to_string(),
        byte_count: 0,
        test_count: 0,
        pass_count: 0,
        fail_count: 0,
    };
    assert!((a.pass_rate() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_artifact_serde_roundtrip() {
    let a = EvidenceArtifact {
        tier: EvidenceTier::T3E2eScenarios,
        artifact_name: "e2e.json".to_string(),
        byte_count: 5000,
        test_count: 50,
        pass_count: 48,
        fail_count: 2,
    };
    let json = serde_json::to_string(&a).unwrap();
    let back: EvidenceArtifact = serde_json::from_str(&json).unwrap();
    assert_eq!(a.tier, back.tier);
    assert_eq!(a.fail_count, back.fail_count);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Evidence completeness checker
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_evidence_checker_passes_with_complete_package() {
    let package = EvidenceBuilder::new("abc123").build_complete_passing();
    let report = check_evidence_completeness(&package);
    assert!(report.is_complete);
    assert_eq!(report.blocking_gaps, 0);
    assert!(gate_passes(&package));
}

#[test]
fn test_evidence_checker_rejects_missing_t1() {
    let mut builder = EvidenceBuilder::new("sha1");
    // Add all tiers except T1
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T1UnitTests {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    assert!(!report.is_complete);
    assert!(report.missing_tiers.contains(&EvidenceTier::T1UnitTests));
}

#[test]
fn test_evidence_checker_rejects_missing_t3_results() {
    let mut builder = EvidenceBuilder::new("sha2");
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T3E2eScenarios {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    assert!(!report.is_complete);
    assert!(report.missing_tiers.contains(&EvidenceTier::T3E2eScenarios));
}

#[test]
fn test_evidence_checker_rejects_missing_rollback_drill() {
    let mut builder = EvidenceBuilder::new("sha3");
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::RollbackDrill {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    assert!(!gate_passes(&package));
}

#[test]
fn test_evidence_checker_rejects_empty_artifact() {
    let mut builder = EvidenceBuilder::new("sha4");
    builder.add_empty(EvidenceTier::T1UnitTests);
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T1UnitTests {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    assert!(!report.is_complete);
    assert_eq!(report.empty_artifacts.len(), 1);
}

#[test]
fn test_evidence_checker_rejects_failing_blocking_tier() {
    let mut builder = EvidenceBuilder::new("sha5");
    builder.add_failing(EvidenceTier::T2IntegrationTests, 20, 3);
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T2IntegrationTests {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    assert!(!report.is_complete);
    assert_eq!(report.failing_tiers.len(), 1);
    assert_eq!(report.failing_tiers[0].0, EvidenceTier::T2IntegrationTests);
}

#[test]
fn test_evidence_checker_allows_failing_advisory_tier() {
    let mut builder = EvidenceBuilder::new("sha6");
    builder.add_failing(EvidenceTier::T6PerfSoak, 10, 2);
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T6PerfSoak {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    // T6 failures don't block
    assert!(report.is_complete);
    assert!(gate_passes(&package));
}

#[test]
fn test_evidence_checker_missing_advisory_is_advisory_gap() {
    let mut builder = EvidenceBuilder::new("sha7");
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T6PerfSoak {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    assert_eq!(report.advisory_gaps, 1);
    // Still passes since T6 is advisory
    assert!(report.is_complete);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Gap report
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_gap_report_lists_all_missing_artifacts() {
    // Provide only T1
    let mut builder = EvidenceBuilder::new("sha8");
    builder.add_passing(EvidenceTier::T1UnitTests, 10);
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    // Missing: T2, T3, T4, T5, T6, MigrationDryRun, RollbackDrill = 7
    assert_eq!(report.missing_tiers.len(), 7);
}

#[test]
fn test_gap_report_empty_for_complete_package() {
    let package = EvidenceBuilder::new("sha9").build_complete_passing();
    let report = check_evidence_completeness(&package);
    assert!(report.missing_tiers.is_empty());
    assert!(report.empty_artifacts.is_empty());
    assert!(report.failing_tiers.is_empty());
}

#[test]
fn test_gap_report_blocking_count() {
    // Missing T1 (blocking) and T6 (advisory) = 1 blocking gap
    let mut builder = EvidenceBuilder::new("sha10");
    for tier in EvidenceTier::all() {
        if tier != EvidenceTier::T1UnitTests && tier != EvidenceTier::T6PerfSoak {
            builder.add_passing(tier, 10);
        }
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    assert_eq!(report.blocking_gaps, 1);
    assert_eq!(report.advisory_gaps, 1);
}

#[test]
fn test_gap_report_combines_missing_empty_and_failing() {
    let mut builder = EvidenceBuilder::new("sha11");
    // Missing: T5, MigrationDryRun, RollbackDrill, T6
    builder.add_passing(EvidenceTier::T1UnitTests, 10);
    builder.add_failing(EvidenceTier::T2IntegrationTests, 10, 2); // failing blocking
    builder.add_empty(EvidenceTier::T3E2eScenarios); // empty
    builder.add_passing(EvidenceTier::T4LoggingAssertions, 10);
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    // missing blocking: T5, MigrationDryRun, RollbackDrill = 3
    // missing advisory: T6 = 1
    // empty artifacts: T3 = 1
    // failing blocking: T2 = 1
    // blocking_gaps = 3 + 1 + 1 = 5
    assert_eq!(report.blocking_gaps, 5);
    assert_eq!(report.advisory_gaps, 1);
    assert!(!report.is_complete);
}

#[test]
fn test_gap_report_serde_roundtrip() {
    let mut builder = EvidenceBuilder::new("sha12");
    builder.add_passing(EvidenceTier::T1UnitTests, 10);
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    let json = serde_json::to_string_pretty(&report).unwrap();
    let back: GapReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report.is_complete, back.is_complete);
    assert_eq!(report.missing_tiers.len(), back.missing_tiers.len());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Gate decision
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_gate_passes_complete() {
    let package = EvidenceBuilder::new("sha-ok").build_complete_passing();
    assert!(gate_passes(&package));
}

#[test]
fn test_gate_fails_incomplete() {
    let package = EvidenceBuilder::new("sha-fail").build();
    assert!(!gate_passes(&package));
}

#[test]
fn test_gate_fails_with_blocking_failures() {
    let mut builder = EvidenceBuilder::new("sha-fail2");
    builder.add_failing(EvidenceTier::T4LoggingAssertions, 10, 1);
    let package = builder.build_complete_passing();
    // T4 was already added by build_complete_passing as passing;
    // but we added it first with failures, so it got overwritten.
    // builder.add_failing then build_complete_passing: build_complete_passing
    // only adds missing tiers, so T4 keeps its failing entry.
    let report = check_evidence_completeness(&package);
    assert!(!report.is_complete);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Evidence package
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_evidence_package_serde_roundtrip() {
    let package = EvidenceBuilder::new("sha-rt").build_complete_passing();
    let json = serde_json::to_string_pretty(&package).unwrap();
    let back: EvidencePackage = serde_json::from_str(&json).unwrap();
    assert_eq!(package.commit_sha, back.commit_sha);
    assert_eq!(package.artifacts.len(), back.artifacts.len());
}

#[test]
fn test_evidence_package_commit_sha_preserved() {
    let package = EvidenceBuilder::new("deadbeef").build_complete_passing();
    assert_eq!(package.commit_sha, "deadbeef");
}

#[test]
fn test_evidence_package_has_8_artifacts_when_complete() {
    let package = EvidenceBuilder::new("sha-full").build_complete_passing();
    assert_eq!(package.artifacts.len(), 8);
}

#[test]
fn test_evidence_builder_add_passing_then_failing_keeps_failing() {
    let mut builder = EvidenceBuilder::new("sha");
    builder.add_passing(EvidenceTier::T1UnitTests, 10);
    builder.add_failing(EvidenceTier::T1UnitTests, 10, 3);
    let package = builder.build();
    let t1 = package.artifacts.iter().find(|a| a.tier == EvidenceTier::T1UnitTests).unwrap();
    assert_eq!(t1.fail_count, 3);
}

#[test]
fn test_evidence_builder_add_failing_then_complete_keeps_failing() {
    let mut builder = EvidenceBuilder::new("sha");
    builder.add_failing(EvidenceTier::T1UnitTests, 10, 2);
    let package = builder.build_complete_passing();
    let t1 = package.artifacts.iter().find(|a| a.tier == EvidenceTier::T1UnitTests).unwrap();
    // build_complete_passing skips already-present tiers
    assert_eq!(t1.fail_count, 2);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Edge cases
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_empty_package_has_8_missing_tiers() {
    let package = EvidencePackage {
        commit_sha: "empty".to_string(),
        artifacts: Vec::new(),
    };
    let report = check_evidence_completeness(&package);
    assert_eq!(report.missing_tiers.len(), 8);
    assert!(!report.is_complete);
}

#[test]
fn test_duplicate_tier_artifacts_deduplicated_in_present_set() {
    // If two artifacts for same tier, tier still counts as present
    let package = EvidencePackage {
        commit_sha: "dup".to_string(),
        artifacts: vec![
            EvidenceArtifact {
                tier: EvidenceTier::T1UnitTests,
                artifact_name: "t1_a.json".to_string(),
                byte_count: 100,
                test_count: 5,
                pass_count: 5,
                fail_count: 0,
            },
            EvidenceArtifact {
                tier: EvidenceTier::T1UnitTests,
                artifact_name: "t1_b.json".to_string(),
                byte_count: 200,
                test_count: 10,
                pass_count: 10,
                fail_count: 0,
            },
        ],
    };
    let report = check_evidence_completeness(&package);
    // T1 present, but T2-T5, T6, MigrationDryRun, RollbackDrill = 7 missing
    assert_eq!(report.missing_tiers.len(), 7);
}

#[test]
fn test_multiple_empty_artifacts_all_reported() {
    let package = EvidencePackage {
        commit_sha: "multi-empty".to_string(),
        artifacts: vec![
            EvidenceArtifact {
                tier: EvidenceTier::T1UnitTests,
                artifact_name: "t1.json".to_string(),
                byte_count: 0,
                test_count: 0,
                pass_count: 0,
                fail_count: 0,
            },
            EvidenceArtifact {
                tier: EvidenceTier::T2IntegrationTests,
                artifact_name: "t2.json".to_string(),
                byte_count: 0,
                test_count: 0,
                pass_count: 0,
                fail_count: 0,
            },
        ],
    };
    let report = check_evidence_completeness(&package);
    assert_eq!(report.empty_artifacts.len(), 2);
}

#[test]
fn test_all_tiers_failing_all_reported() {
    let mut builder = EvidenceBuilder::new("sha-allf");
    for tier in EvidenceTier::all() {
        builder.add_failing(tier, 10, 1);
    }
    let package = builder.build();
    let report = check_evidence_completeness(&package);
    // 7 blocking tiers failing
    assert_eq!(report.failing_tiers.len(), 7);
}

#[test]
fn test_migration_dry_run_is_blocking() {
    assert!(EvidenceTier::MigrationDryRun.is_blocking());
}

#[test]
fn test_rollback_drill_is_blocking() {
    assert!(EvidenceTier::RollbackDrill.is_blocking());
}
