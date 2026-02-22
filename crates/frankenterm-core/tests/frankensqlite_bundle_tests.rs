//! E4.F2.T4: Validation artifact bundling, log-schema checks, and triage summaries.
//!
//! Tests the bundle collector, triage summary generator, log-field coverage
//! checker, and JSON output format for go/no-go review artifacts.

use std::collections::HashMap;

// ═══════════════════════════════════════════════════════════════════════
// Validation bundle model
// ═══════════════════════════════════════════════════════════════════════

/// Schema version for the validation bundle format.
const BUNDLE_SCHEMA_VERSION: &str = "ft.validation-bundle.v1";

/// Required structured log fields that must appear in migration pipelines.
const REQUIRED_LOG_FIELDS: &[&str] = &[
    "migration_stage",
    "event_count",
    "digest",
    "events_exported",
    "rollback_class",
    "backend",
    "data_path",
];

/// A single tier's test result within the bundle.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct TierResult {
    tier: String,
    passed: u32,
    failed: u32,
    skipped: u32,
    duration_s: f64,
    blocking: bool,
    failures: Vec<String>,
}

impl TierResult {
    fn is_green(&self) -> bool {
        self.failed == 0
    }
}

/// Performance benchmark result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct PerfResult {
    metric: String,
    p50_ms: f64,
    p99_ms: f64,
    budget_ms: f64,
    within_budget: bool,
}

/// Log field coverage entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct LogFieldCoverage {
    field: String,
    present: bool,
    occurrences: u32,
    source_stages: Vec<String>,
}

/// Triage summary for go/no-go decision.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct TriageSummary {
    overall: String,          // "pass" | "fail" | "advisory_warn"
    tiers_passed: u32,
    tiers_failed: u32,
    blocking_failures: Vec<String>,
    advisory_warnings: Vec<String>,
    wave_readiness: String,   // BT tier label (BT1..BT5)
    recommendation: String,
}

/// The complete validation bundle for a go/no-go review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
struct ValidationBundle {
    bundle_schema_version: String,
    timestamp: String,
    commit_sha: String,
    correlation_id: String,
    tier_results: Vec<TierResult>,
    perf_results: Vec<PerfResult>,
    log_field_coverage: Vec<LogFieldCoverage>,
    triage_summary: TriageSummary,
}

// ═══════════════════════════════════════════════════════════════════════
// Bundle builder
// ═══════════════════════════════════════════════════════════════════════

struct BundleBuilder {
    commit_sha: String,
    correlation_id: String,
    tier_results: Vec<TierResult>,
    perf_results: Vec<PerfResult>,
    log_fields_seen: HashMap<String, (u32, Vec<String>)>,
}

impl BundleBuilder {
    fn new(commit_sha: &str, correlation_id: &str) -> Self {
        Self {
            commit_sha: commit_sha.to_string(),
            correlation_id: correlation_id.to_string(),
            tier_results: Vec::new(),
            perf_results: Vec::new(),
            log_fields_seen: HashMap::new(),
        }
    }

    fn add_tier_result(&mut self, result: TierResult) {
        self.tier_results.push(result);
    }

    fn add_perf_result(&mut self, result: PerfResult) {
        self.perf_results.push(result);
    }

    fn record_log_field(&mut self, field: &str, stage: &str) {
        let entry = self
            .log_fields_seen
            .entry(field.to_string())
            .or_insert_with(|| (0, Vec::new()));
        entry.0 += 1;
        if !entry.1.contains(&stage.to_string()) {
            entry.1.push(stage.to_string());
        }
    }

    fn build_log_coverage(&self) -> Vec<LogFieldCoverage> {
        REQUIRED_LOG_FIELDS
            .iter()
            .map(|&field| {
                let (count, stages) = self
                    .log_fields_seen
                    .get(field)
                    .cloned()
                    .unwrap_or((0, Vec::new()));
                LogFieldCoverage {
                    field: field.to_string(),
                    present: count > 0,
                    occurrences: count,
                    source_stages: stages,
                }
            })
            .collect()
    }

    fn build_triage(&self) -> TriageSummary {
        let passed = self.tier_results.iter().filter(|t| t.is_green()).count() as u32;
        let failed = self.tier_results.len() as u32 - passed;

        let blocking_failures: Vec<String> = self
            .tier_results
            .iter()
            .filter(|t| t.blocking && !t.is_green())
            .map(|t| t.tier.clone())
            .collect();

        let advisory_warnings: Vec<String> = self
            .tier_results
            .iter()
            .filter(|t| !t.blocking && !t.is_green())
            .map(|t| t.tier.clone())
            .collect();

        let overall = if blocking_failures.is_empty() {
            if advisory_warnings.is_empty() {
                "pass"
            } else {
                "advisory_warn"
            }
        } else {
            "fail"
        };

        // Determine wave readiness: highest BT achievable
        let wave = if blocking_failures.is_empty() {
            if advisory_warnings.is_empty() {
                "BT5"
            } else {
                "BT3"
            }
        } else {
            // Check which tiers passed to determine wave
            let t1_ok = self.tier_results.iter().any(|t| t.tier == "T1" && t.is_green());
            let t2_ok = self.tier_results.iter().any(|t| t.tier == "T2" && t.is_green());
            if t1_ok && t2_ok {
                "BT2"
            } else if t1_ok {
                "BT1"
            } else {
                "BT0"
            }
        };

        let recommendation = match overall {
            "pass" => "Proceed with rollout".to_string(),
            "advisory_warn" => "Proceed with caution; review advisory failures".to_string(),
            _ => format!(
                "Block rollout; fix {} blocking failure(s)",
                blocking_failures.len()
            ),
        };

        TriageSummary {
            overall: overall.to_string(),
            tiers_passed: passed,
            tiers_failed: failed,
            blocking_failures,
            advisory_warnings,
            wave_readiness: wave.to_string(),
            recommendation,
        }
    }

    fn build(self) -> ValidationBundle {
        let log_coverage = self.build_log_coverage();
        let triage = self.build_triage();
        ValidationBundle {
            bundle_schema_version: BUNDLE_SCHEMA_VERSION.to_string(),
            timestamp: "2026-02-22T11:00:00Z".to_string(),
            commit_sha: self.commit_sha,
            correlation_id: self.correlation_id,
            tier_results: self.tier_results,
            perf_results: self.perf_results,
            log_field_coverage: log_coverage,
            triage_summary: triage,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Test helpers
// ═══════════════════════════════════════════════════════════════════════

fn green_tier(tier: &str, blocking: bool) -> TierResult {
    TierResult {
        tier: tier.to_string(),
        passed: 30,
        failed: 0,
        skipped: 0,
        duration_s: 5.0,
        blocking,
        failures: vec![],
    }
}

fn red_tier(tier: &str, blocking: bool, failures: Vec<&str>) -> TierResult {
    TierResult {
        tier: tier.to_string(),
        passed: 25,
        failed: failures.len() as u32,
        skipped: 0,
        duration_s: 8.0,
        blocking,
        failures: failures.into_iter().map(|s| s.to_string()).collect(),
    }
}

fn build_all_green_bundle() -> ValidationBundle {
    let mut builder = BundleBuilder::new("abc123def", "corr-2026-001");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(green_tier("T2", true));
    builder.add_tier_result(green_tier("T3", true));
    builder.add_tier_result(green_tier("T4", true));
    builder.add_tier_result(green_tier("T5", true));
    builder.add_tier_result(green_tier("T6", false));

    builder.add_perf_result(PerfResult {
        metric: "append_p50".to_string(),
        p50_ms: 0.5,
        p99_ms: 4.0,
        budget_ms: 5.0,
        within_budget: true,
    });

    for field in REQUIRED_LOG_FIELDS {
        builder.record_log_field(field, "M0");
        builder.record_log_field(field, "M1");
    }

    builder.build()
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Bundle structure and schema
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_bundle_schema_version() {
    assert_eq!(BUNDLE_SCHEMA_VERSION, "ft.validation-bundle.v1");
}

#[test]
fn test_bundle_output_format_valid_json() {
    let bundle = build_all_green_bundle();
    let json = serde_json::to_string_pretty(&bundle).unwrap();
    let reparsed: ValidationBundle = serde_json::from_str(&json).unwrap();
    assert_eq!(bundle, reparsed);
}

#[test]
fn test_bundle_includes_correlation_ids() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.correlation_id, "corr-2026-001");
    assert_eq!(bundle.commit_sha, "abc123def");
}

#[test]
fn test_bundle_collects_all_tier_results() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.tier_results.len(), 6);
    let tier_names: Vec<&str> = bundle.tier_results.iter().map(|t| t.tier.as_str()).collect();
    assert_eq!(tier_names, vec!["T1", "T2", "T3", "T4", "T5", "T6"]);
}

#[test]
fn test_bundle_timestamp_present() {
    let bundle = build_all_green_bundle();
    assert!(!bundle.timestamp.is_empty());
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Triage summary
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_bundle_triage_summary_all_green() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.triage_summary.overall, "pass");
    assert_eq!(bundle.triage_summary.tiers_passed, 6);
    assert_eq!(bundle.triage_summary.tiers_failed, 0);
    assert!(bundle.triage_summary.blocking_failures.is_empty());
}

#[test]
fn test_bundle_triage_summary_blocking_failure() {
    let mut builder = BundleBuilder::new("sha1", "corr-1");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(red_tier("T2", true, vec!["test_migration_fail"]));
    builder.add_tier_result(green_tier("T3", true));
    builder.add_tier_result(green_tier("T4", true));
    builder.add_tier_result(green_tier("T5", true));
    builder.add_tier_result(green_tier("T6", false));

    let bundle = builder.build();
    assert_eq!(bundle.triage_summary.overall, "fail");
    assert_eq!(bundle.triage_summary.blocking_failures, vec!["T2"]);
}

#[test]
fn test_bundle_triage_summary_advisory_only() {
    let mut builder = BundleBuilder::new("sha2", "corr-2");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(green_tier("T2", true));
    builder.add_tier_result(green_tier("T3", true));
    builder.add_tier_result(green_tier("T4", true));
    builder.add_tier_result(green_tier("T5", true));
    builder.add_tier_result(red_tier("T6", false, vec!["test_slo_p99_exceeded"]));

    let bundle = builder.build();
    assert_eq!(bundle.triage_summary.overall, "advisory_warn");
    assert!(bundle.triage_summary.blocking_failures.is_empty());
    assert_eq!(bundle.triage_summary.advisory_warnings, vec!["T6"]);
}

#[test]
fn test_bundle_triage_wave_readiness_bt5() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.triage_summary.wave_readiness, "BT5");
}

#[test]
fn test_bundle_triage_wave_readiness_bt3_on_advisory_fail() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(green_tier("T2", true));
    builder.add_tier_result(green_tier("T3", true));
    builder.add_tier_result(green_tier("T4", true));
    builder.add_tier_result(green_tier("T5", true));
    builder.add_tier_result(red_tier("T6", false, vec!["perf"]));

    let bundle = builder.build();
    assert_eq!(bundle.triage_summary.wave_readiness, "BT3");
}

#[test]
fn test_bundle_triage_wave_readiness_bt2_on_t3_fail() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(green_tier("T2", true));
    builder.add_tier_result(red_tier("T3", true, vec!["rollback"]));

    let bundle = builder.build();
    assert_eq!(bundle.triage_summary.wave_readiness, "BT2");
}

#[test]
fn test_bundle_triage_wave_readiness_bt1_on_t2_fail() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(red_tier("T2", true, vec!["e2e"]));

    let bundle = builder.build();
    assert_eq!(bundle.triage_summary.wave_readiness, "BT1");
}

#[test]
fn test_bundle_triage_wave_readiness_bt0_on_t1_fail() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(red_tier("T1", true, vec!["contract"]));

    let bundle = builder.build();
    assert_eq!(bundle.triage_summary.wave_readiness, "BT0");
}

#[test]
fn test_bundle_triage_recommendation_pass() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.triage_summary.recommendation, "Proceed with rollout");
}

#[test]
fn test_bundle_triage_recommendation_advisory() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(red_tier("T6", false, vec!["perf"]));
    let bundle = builder.build();
    assert!(bundle.triage_summary.recommendation.contains("caution"));
}

#[test]
fn test_bundle_triage_recommendation_block() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(red_tier("T1", true, vec!["fail"]));
    let bundle = builder.build();
    assert!(bundle.triage_summary.recommendation.contains("Block"));
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Log field coverage
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_bundle_log_coverage_all_required_fields() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.log_field_coverage.len(), REQUIRED_LOG_FIELDS.len());
}

#[test]
fn test_bundle_log_coverage_all_present_when_recorded() {
    let bundle = build_all_green_bundle();
    for cov in &bundle.log_field_coverage {
        assert!(cov.present, "field {} should be present", cov.field);
        assert!(cov.occurrences > 0);
    }
}

#[test]
fn test_bundle_log_coverage_missing_field_detected() {
    let mut builder = BundleBuilder::new("sha", "corr");
    // Only record some fields
    builder.record_log_field("migration_stage", "M0");
    builder.record_log_field("event_count", "M0");
    let bundle = builder.build();

    let missing: Vec<&str> = bundle
        .log_field_coverage
        .iter()
        .filter(|c| !c.present)
        .map(|c| c.field.as_str())
        .collect();
    assert!(missing.contains(&"digest"));
    assert!(missing.contains(&"backend"));
}

#[test]
fn test_bundle_log_coverage_tracks_source_stages() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.record_log_field("migration_stage", "M0");
    builder.record_log_field("migration_stage", "M1");
    builder.record_log_field("migration_stage", "M2");
    let bundle = builder.build();

    let stage_cov = bundle
        .log_field_coverage
        .iter()
        .find(|c| c.field == "migration_stage")
        .unwrap();
    assert_eq!(stage_cov.source_stages.len(), 3);
    assert!(stage_cov.source_stages.contains(&"M0".to_string()));
}

#[test]
fn test_bundle_log_coverage_dedup_stages() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.record_log_field("event_count", "M1");
    builder.record_log_field("event_count", "M1"); // Same stage again
    builder.record_log_field("event_count", "M2");
    let bundle = builder.build();

    let cov = bundle
        .log_field_coverage
        .iter()
        .find(|c| c.field == "event_count")
        .unwrap();
    assert_eq!(cov.occurrences, 3);
    assert_eq!(cov.source_stages.len(), 2); // Deduped
}

#[test]
fn test_required_log_fields_count() {
    assert_eq!(REQUIRED_LOG_FIELDS.len(), 7);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Perf results
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_bundle_perf_results_included() {
    let bundle = build_all_green_bundle();
    assert_eq!(bundle.perf_results.len(), 1);
    assert_eq!(bundle.perf_results[0].metric, "append_p50");
}

#[test]
fn test_bundle_perf_within_budget_check() {
    let result = PerfResult {
        metric: "flush_p99".to_string(),
        p50_ms: 10.0,
        p99_ms: 45.0,
        budget_ms: 50.0,
        within_budget: true,
    };
    assert!(result.within_budget);
    assert!(result.p99_ms < result.budget_ms);
}

#[test]
fn test_bundle_perf_over_budget() {
    let result = PerfResult {
        metric: "flush_p99".to_string(),
        p50_ms: 30.0,
        p99_ms: 75.0,
        budget_ms: 50.0,
        within_budget: false,
    };
    assert!(!result.within_budget);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Tier result details
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_tier_result_is_green_when_zero_failures() {
    let tier = green_tier("T1", true);
    assert!(tier.is_green());
}

#[test]
fn test_tier_result_not_green_when_failures() {
    let tier = red_tier("T1", true, vec!["test_fail"]);
    assert!(!tier.is_green());
}

#[test]
fn test_tier_result_failures_list() {
    let tier = red_tier("T2", true, vec!["test_a", "test_b"]);
    assert_eq!(tier.failures.len(), 2);
    assert_eq!(tier.failed, 2);
}

#[test]
fn test_tier_result_serde_roundtrip() {
    let tier = red_tier("T3", true, vec!["test_rollback"]);
    let json = serde_json::to_string(&tier).unwrap();
    let back: TierResult = serde_json::from_str(&json).unwrap();
    assert_eq!(tier, back);
}

// ═══════════════════════════════════════════════════════════════════════
// Tests: Bundle builder
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn test_builder_new_sets_correlation() {
    let builder = BundleBuilder::new("sha123", "corr-abc");
    let bundle = builder.build();
    assert_eq!(bundle.commit_sha, "sha123");
    assert_eq!(bundle.correlation_id, "corr-abc");
}

#[test]
fn test_builder_empty_produces_valid_bundle() {
    let builder = BundleBuilder::new("sha", "corr");
    let bundle = builder.build();
    assert!(bundle.tier_results.is_empty());
    assert!(bundle.perf_results.is_empty());
    assert_eq!(bundle.triage_summary.overall, "pass"); // No failures
}

#[test]
fn test_builder_multiple_tiers() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(green_tier("T2", true));
    let bundle = builder.build();
    assert_eq!(bundle.tier_results.len(), 2);
}

#[test]
fn test_bundle_full_schema_roundtrip() {
    let bundle = build_all_green_bundle();
    let json = serde_json::to_string(&bundle).unwrap();
    // Verify it's valid JSON
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(value.is_object());
    assert!(value.get("bundle_schema_version").is_some());
    assert!(value.get("tier_results").unwrap().is_array());
    assert!(value.get("triage_summary").unwrap().is_object());
}

#[test]
fn test_bundle_json_has_all_top_level_keys() {
    let bundle = build_all_green_bundle();
    let json = serde_json::to_string(&bundle).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let obj = value.as_object().unwrap();
    let expected_keys = [
        "bundle_schema_version",
        "timestamp",
        "commit_sha",
        "correlation_id",
        "tier_results",
        "perf_results",
        "log_field_coverage",
        "triage_summary",
    ];
    for key in &expected_keys {
        assert!(obj.contains_key(*key), "missing key: {key}");
    }
}

#[test]
fn test_bundle_triage_counts_match_tiers() {
    let mut builder = BundleBuilder::new("sha", "corr");
    builder.add_tier_result(green_tier("T1", true));
    builder.add_tier_result(red_tier("T2", true, vec!["fail"]));
    builder.add_tier_result(green_tier("T3", true));
    let bundle = builder.build();
    assert_eq!(
        bundle.triage_summary.tiers_passed + bundle.triage_summary.tiers_failed,
        bundle.tier_results.len() as u32
    );
}
