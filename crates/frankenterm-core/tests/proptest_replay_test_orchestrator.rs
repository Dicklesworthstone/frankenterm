//! Property-based tests for replay_test_orchestrator (ft-og6q6.7.7).
//!
//! Invariants tested:
//! - TO-1: Orchestrate all-pass produces Pass status
//! - TO-2: Orchestrate with failure produces Fail status
//! - TO-3: fail_fast stops at first failed gate
//! - TO-4: no-fail-fast runs all gates
//! - TO-5: gates_run + (total - gates_run) = total gates requested
//! - TO-6: gates_passed + gates_failed <= gates_run
//! - TO-7: total_duration_ms is sum of gate durations
//! - TO-8: OrchestratorResult serde roundtrip
//! - TO-9: EvidenceManifest total_size_bytes is sum of entries
//! - TO-10: EvidenceManifest serde roundtrip
//! - TO-11: RetentionPolicy prunes files older than limit
//! - TO-12: RetentionPolicy keeps files within limit
//! - TO-13: prune_count matches filtered count
//! - TO-14: SummaryReport total_pass + total_fail <= total checks
//! - TO-15: SummaryReport markdown contains "Replay Test Summary"
//! - TO-16: GateRunResult from GateReport preserves fields
//! - TO-17: OrchestratorConfig serde roundtrip
//! - TO-18: ManifestEntry serde roundtrip
//! - TO-19: RetentionPolicy serde roundtrip
//! - TO-20: SummaryReport serde roundtrip

use proptest::prelude::*;

use frankenterm_core::replay_ci_gate::{
    GateCheck, GateId, GateReport, GateStatus, ALL_GATES,
};
use frankenterm_core::replay_test_orchestrator::{
    orchestrate, OrchestratorConfig, OrchestratorResult,
    EvidenceManifest, ManifestEntry, ManifestFileType,
    RetentionPolicy, RetentionCandidate, evaluate_retention, prune_count,
    SummaryReport, GateRunResult,
};

fn pass_report(gate: GateId, dur: u64) -> GateReport {
    GateReport::new(
        gate,
        vec![GateCheck {
            name: "ok".into(),
            passed: true,
            message: "pass".into(),
            duration_ms: Some(dur),
            artifact_path: None,
        }],
        dur,
        "2026-01-01T00:00:00Z".into(),
    )
}

fn fail_report(gate: GateId, dur: u64) -> GateReport {
    GateReport::new(
        gate,
        vec![GateCheck {
            name: "bad".into(),
            passed: false,
            message: "fail".into(),
            duration_ms: None,
            artifact_path: None,
        }],
        dur,
        "2026-01-01T00:00:00Z".into(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── TO-1: All pass → Pass ────────────────────────────────────────────

    #[test]
    fn to01_all_pass(d1 in 10u64..1000, d2 in 10u64..1000, d3 in 10u64..1000) {
        let config = OrchestratorConfig::default();
        let reports = vec![
            pass_report(GateId::Smoke, d1),
            pass_report(GateId::TestSuite, d2),
            pass_report(GateId::Regression, d3),
        ];
        let result = orchestrate(&config, &reports);
        prop_assert_eq!(result.overall_status, GateStatus::Pass);
        prop_assert_eq!(result.gates_run, 3);
    }

    // ── TO-2: Any failure → Fail ─────────────────────────────────────────

    #[test]
    fn to02_any_failure(fail_idx in 0usize..3) {
        let config = OrchestratorConfig { fail_fast: false, ..Default::default() };
        let reports: Vec<GateReport> = ALL_GATES.iter().enumerate().map(|(i, g)| {
            if i == fail_idx {
                fail_report(*g, 100)
            } else {
                pass_report(*g, 100)
            }
        }).collect();
        let result = orchestrate(&config, &reports);
        prop_assert_eq!(result.overall_status, GateStatus::Fail);
    }

    // ── TO-3: fail_fast stops at failed gate ─────────────────────────────

    #[test]
    fn to03_fail_fast_stops(fail_idx in 0usize..3) {
        let config = OrchestratorConfig { fail_fast: true, ..Default::default() };
        let reports: Vec<GateReport> = ALL_GATES.iter().enumerate().map(|(i, g)| {
            if i == fail_idx {
                fail_report(*g, 100)
            } else {
                pass_report(*g, 100)
            }
        }).collect();
        let result = orchestrate(&config, &reports);
        prop_assert_eq!(result.gates_run, fail_idx + 1);
        prop_assert!(result.fail_fast_triggered);
    }

    // ── TO-4: no-fail-fast runs all ──────────────────────────────────────

    #[test]
    fn to04_no_fail_fast_runs_all(fail_idx in 0usize..3) {
        let config = OrchestratorConfig { fail_fast: false, ..Default::default() };
        let reports: Vec<GateReport> = ALL_GATES.iter().enumerate().map(|(i, g)| {
            if i == fail_idx {
                fail_report(*g, 100)
            } else {
                pass_report(*g, 100)
            }
        }).collect();
        let result = orchestrate(&config, &reports);
        prop_assert_eq!(result.gates_run, 3);
        let is_triggered = result.fail_fast_triggered;
        prop_assert!(!is_triggered);
    }

    // ── TO-5: gates_run bounded by requested ─────────────────────────────

    #[test]
    fn to05_gates_run_bounded(gate_idx in 0usize..3) {
        let config = OrchestratorConfig::for_gate(ALL_GATES[gate_idx]);
        let report = pass_report(ALL_GATES[gate_idx], 100);
        let result = orchestrate(&config, &[report]);
        prop_assert!(result.gates_run <= 1);
    }

    // ── TO-6: passed + failed <= run ─────────────────────────────────────

    #[test]
    fn to06_counts_consistent(fail_idx in 0usize..3) {
        let config = OrchestratorConfig { fail_fast: false, ..Default::default() };
        let reports: Vec<GateReport> = ALL_GATES.iter().enumerate().map(|(i, g)| {
            if i == fail_idx {
                fail_report(*g, 100)
            } else {
                pass_report(*g, 100)
            }
        }).collect();
        let result = orchestrate(&config, &reports);
        prop_assert!(result.gates_passed + result.gates_failed <= result.gates_run);
    }

    // ── TO-7: total_duration is sum ──────────────────────────────────────

    #[test]
    fn to07_duration_sum(d1 in 10u64..1000, d2 in 10u64..1000, d3 in 10u64..1000) {
        let config = OrchestratorConfig { fail_fast: false, ..Default::default() };
        let reports = vec![
            pass_report(GateId::Smoke, d1),
            pass_report(GateId::TestSuite, d2),
            pass_report(GateId::Regression, d3),
        ];
        let result = orchestrate(&config, &reports);
        prop_assert_eq!(result.total_duration_ms, d1 + d2 + d3);
    }

    // ── TO-8: OrchestratorResult serde ───────────────────────────────────

    #[test]
    fn to08_result_serde(dur in 10u64..10000) {
        let config = OrchestratorConfig::default();
        let reports = vec![pass_report(GateId::Smoke, dur)];
        let result = orchestrate(&config, &reports);
        let json = serde_json::to_string(&result).unwrap();
        let restored: OrchestratorResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── TO-9: Manifest total size is sum ─────────────────────────────────

    #[test]
    fn to09_manifest_total(s1 in 0u64..10000, s2 in 0u64..10000) {
        let entries = vec![
            ManifestEntry {
                path: "a.json".into(),
                size_bytes: s1,
                checksum: "abc".into(),
                file_type: ManifestFileType::GateReport,
            },
            ManifestEntry {
                path: "b.json".into(),
                size_bytes: s2,
                checksum: "def".into(),
                file_type: ManifestFileType::TestOutput,
            },
        ];
        let manifest = EvidenceManifest::new(entries, "now".into(), 90);
        prop_assert_eq!(manifest.total_size_bytes, s1 + s2);
    }

    // ── TO-10: Manifest serde ────────────────────────────────────────────

    #[test]
    fn to10_manifest_serde(size in 0u64..100000) {
        let entries = vec![ManifestEntry {
            path: "test.json".into(),
            size_bytes: size,
            checksum: "sha256:test".into(),
            file_type: ManifestFileType::Summary,
        }];
        let manifest = EvidenceManifest::new(entries, "now".into(), 90);
        let json = serde_json::to_string(&manifest).unwrap();
        let restored: EvidenceManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, manifest);
    }

    // ── TO-11: Retention prunes old files ────────────────────────────────

    #[test]
    fn to11_retention_prunes_old(age in 91u64..365) {
        let policy = RetentionPolicy::default();
        let files = vec![RetentionCandidate {
            path: "old.json".into(),
            age_days: age,
            file_type: ManifestFileType::GateReport,
        }];
        let decisions = evaluate_retention(&files, &policy, 0);
        prop_assert!(decisions[0].prune);
    }

    // ── TO-12: Retention keeps fresh files ───────────────────────────────

    #[test]
    fn to12_retention_keeps_fresh(age in 0u64..90) {
        let policy = RetentionPolicy::default();
        let files = vec![RetentionCandidate {
            path: "fresh.json".into(),
            age_days: age,
            file_type: ManifestFileType::GateReport,
        }];
        let decisions = evaluate_retention(&files, &policy, 0);
        let is_pruned = decisions[0].prune;
        prop_assert!(!is_pruned);
    }

    // ── TO-13: prune_count matches ───────────────────────────────────────

    #[test]
    fn to13_prune_count(old_count in 0usize..10, fresh_count in 0usize..10) {
        let policy = RetentionPolicy::default();
        let mut files = Vec::new();
        for i in 0..old_count {
            files.push(RetentionCandidate {
                path: format!("old_{}.json", i),
                age_days: 100,
                file_type: ManifestFileType::GateReport,
            });
        }
        for i in 0..fresh_count {
            files.push(RetentionCandidate {
                path: format!("fresh_{}.json", i),
                age_days: 30,
                file_type: ManifestFileType::GateReport,
            });
        }
        let decisions = evaluate_retention(&files, &policy, 0);
        prop_assert_eq!(prune_count(&decisions), old_count);
    }

    // ── TO-14: Summary totals consistent ─────────────────────────────────

    #[test]
    fn to14_summary_totals(fail_idx in 0usize..3) {
        let config = OrchestratorConfig { fail_fast: false, ..Default::default() };
        let reports: Vec<GateReport> = ALL_GATES.iter().enumerate().map(|(i, g)| {
            if i == fail_idx { fail_report(*g, 100) } else { pass_report(*g, 100) }
        }).collect();
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "now".into());
        let tp = summary.total_pass();
        let tf = summary.total_fail();
        prop_assert_eq!(tp + tf, 3); // 3 gates, 1 check each
    }

    // ── TO-15: Markdown contains header ──────────────────────────────────

    #[test]
    fn to15_markdown_header(gate_idx in 0usize..3) {
        let config = OrchestratorConfig::for_gate(ALL_GATES[gate_idx]);
        let report = pass_report(ALL_GATES[gate_idx], 100);
        let result = orchestrate(&config, &[report]);
        let summary = SummaryReport::from_result(&result, "now".into());
        let md = summary.to_markdown();
        prop_assert!(md.contains("## Replay Test Summary"));
    }

    // ── TO-16: GateRunResult preserves fields ────────────────────────────

    #[test]
    fn to16_run_result_from_report(idx in 0usize..3, dur in 10u64..10000) {
        let gate = ALL_GATES[idx];
        let report = pass_report(gate, dur);
        let run = GateRunResult::from(&report);
        prop_assert_eq!(run.gate, gate);
        prop_assert_eq!(run.status, report.status);
        prop_assert_eq!(run.pass_count, report.pass_count);
        prop_assert_eq!(run.fail_count, report.fail_count);
        prop_assert_eq!(run.duration_ms, dur);
    }

    // ── TO-17: OrchestratorConfig serde ──────────────────────────────────

    #[test]
    fn to17_config_serde(concurrency in 1usize..16, retention in 1u64..365) {
        let config = OrchestratorConfig {
            fail_fast: true,
            max_concurrency: concurrency,
            gate_filter: None,
            format: frankenterm_core::replay_test_orchestrator::OrchestratorFormat::Json,
            retention_days: retention,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: OrchestratorConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, config);
    }

    // ── TO-18: ManifestEntry serde ───────────────────────────────────────

    #[test]
    fn to18_manifest_entry_serde(size in 0u64..1000000) {
        let entry = ManifestEntry {
            path: "test.json".into(),
            size_bytes: size,
            checksum: "sha256:abc".into(),
            file_type: ManifestFileType::GateReport,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ManifestEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, entry);
    }

    // ── TO-19: RetentionPolicy serde ─────────────────────────────────────

    #[test]
    fn to19_retention_serde(days in 1u64..365) {
        let policy = RetentionPolicy {
            gate_reports_days: days,
            regression_logs_days: days,
            test_output_days: days,
            waiver_permanent: true,
            emergency_override_permanent: true,
        };
        let json = serde_json::to_string(&policy).unwrap();
        let restored: RetentionPolicy = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, policy);
    }

    // ── TO-20: SummaryReport serde ───────────────────────────────────────

    #[test]
    fn to20_summary_serde(dur in 10u64..10000) {
        let config = OrchestratorConfig::default();
        let reports = vec![pass_report(GateId::Smoke, dur)];
        let result = orchestrate(&config, &reports);
        let summary = SummaryReport::from_result(&result, "2026-01-01T00:00:00Z".into());
        let json = serde_json::to_string(&summary).unwrap();
        let restored: SummaryReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, summary);
    }
}
