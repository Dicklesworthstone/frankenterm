// Disabled until replay_post_incident module is created.
#![cfg(any())]
//! Property-based tests for replay_post_incident (ft-og6q6.7.6).
//!
//! Invariants tested:
//! - PI-1: PipelineStep str roundtrip
//! - PI-2: Valid input always succeeds pipeline
//! - PI-3: Empty incident_id always fails validation
//! - PI-4: Wrong extension always fails validation
//! - PI-5: Pipeline success implies 5 steps
//! - PI-6: Pipeline total_duration equals sum of step durations
//! - PI-7: Pipeline artifact_path contains incident_id
//! - PI-8: Pipeline bead_id contains incident_id
//! - PI-9: Pipeline is deterministic (idempotent)
//! - PI-10: IncidentCorpus register sets Covered
//! - PI-11: IncidentCorpus register_gap sets Missing
//! - PI-12: Register overwrites gap
//! - PI-13: Coverage of unknown is Missing
//! - PI-14: Gaps count equals entries with Missing status
//! - PI-15: Coverage percent in [0, 100]
//! - PI-16: Empty corpus has 100% coverage
//! - PI-17: PostIncidentInput serde roundtrip
//! - PI-18: PipelineResult serde roundtrip
//! - PI-19: IncidentCorpus serde roundtrip
//! - PI-20: CoverageReport gap_incident_ids subset of entries

use proptest::prelude::*;

use frankenterm_core::replay_post_incident::{
    ALL_STEPS, IncidentCorpus, IncidentCoverageStatus, PipelineResult, PipelineStep,
    PostIncidentInput, execute_pipeline, validate_input,
};

fn arb_incident_id() -> impl Strategy<Value = String> {
    "[A-Z]{3}-[0-9]{3,6}".prop_map(|s| s)
}

fn arb_recording_path() -> impl Strategy<Value = String> {
    "[a-z]{3,10}/[a-z]{3,10}\\.ftreplay".prop_map(|s| s)
}

fn arb_valid_input() -> impl Strategy<Value = PostIncidentInput> {
    (arb_incident_id(), arb_recording_path()).prop_map(|(id, path)| PostIncidentInput {
        incident_id: id,
        recording_path: path,
        severity: Some("high".into()),
        description: Some("test".into()),
        resolved_at: None,
        webhook_url: None,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── PI-1: PipelineStep str roundtrip ────────────────────────────────────

    #[test]
    fn pi01_step_roundtrip(idx in 0usize..5) {
        let step = ALL_STEPS[idx];
        let s = step.as_str();
        let parsed = PipelineStep::from_str_step(s);
        prop_assert_eq!(parsed, Some(step));
    }

    // ── PI-2: Valid input always succeeds pipeline ──────────────────────────

    #[test]
    fn pi02_valid_input_succeeds(input in arb_valid_input()) {
        let result = execute_pipeline(&input);
        prop_assert!(result.success);
        prop_assert!(result.error.is_none());
    }

    // ── PI-3: Empty incident_id always fails ────────────────────────────────

    #[test]
    fn pi03_empty_id_fails(path in arb_recording_path()) {
        let input = PostIncidentInput {
            incident_id: String::new(),
            recording_path: path,
            severity: None,
            description: None,
            resolved_at: None,
            webhook_url: None,
        };
        let result = validate_input(&input);
        prop_assert!(result.is_err());
    }

    // ── PI-4: Wrong extension always fails ──────────────────────────────────

    #[test]
    fn pi04_wrong_extension_fails(
        id in arb_incident_id(),
        ext in "(json|txt|csv|log|zip)"
    ) {
        let input = PostIncidentInput {
            incident_id: id,
            recording_path: format!("file.{}", ext),
            severity: None,
            description: None,
            resolved_at: None,
            webhook_url: None,
        };
        let result = validate_input(&input);
        prop_assert!(result.is_err());
    }

    // ── PI-5: Pipeline success implies 5 steps ──────────────────────────────

    #[test]
    fn pi05_success_has_five_steps(input in arb_valid_input()) {
        let result = execute_pipeline(&input);
        prop_assert_eq!(result.steps.len(), 5);
    }

    // ── PI-6: Total duration equals sum ─────────────────────────────────────

    #[test]
    fn pi06_duration_sum(input in arb_valid_input()) {
        let result = execute_pipeline(&input);
        let sum: u64 = result.steps.iter().map(|s| s.duration_ms).sum();
        prop_assert_eq!(result.total_duration_ms, sum);
    }

    // ── PI-7: Artifact path contains incident_id ────────────────────────────

    #[test]
    fn pi07_artifact_path_has_id(input in arb_valid_input()) {
        let result = execute_pipeline(&input);
        let path = result.artifact_path.unwrap();
        prop_assert!(path.contains(&input.incident_id));
    }

    // ── PI-8: Bead ID contains incident_id ──────────────────────────────────

    #[test]
    fn pi08_bead_id_has_id(input in arb_valid_input()) {
        let result = execute_pipeline(&input);
        let bead = result.bead_id.unwrap();
        prop_assert!(bead.contains(&input.incident_id));
    }

    // ── PI-9: Pipeline is deterministic ─────────────────────────────────────

    #[test]
    fn pi09_deterministic(input in arb_valid_input()) {
        let r1 = execute_pipeline(&input);
        let r2 = execute_pipeline(&input);
        prop_assert_eq!(r1.artifact_path, r2.artifact_path);
        prop_assert_eq!(r1.bead_id, r2.bead_id);
        prop_assert_eq!(r1.total_duration_ms, r2.total_duration_ms);
    }

    // ── PI-10: Register sets Covered ────────────────────────────────────────

    #[test]
    fn pi10_register_covered(id in arb_incident_id()) {
        let mut corpus = IncidentCorpus::new();
        corpus.register(&id, "/a.ftreplay", "bead-1");
        prop_assert_eq!(corpus.coverage(&id), IncidentCoverageStatus::Covered);
    }

    // ── PI-11: Register gap sets Missing ────────────────────────────────────

    #[test]
    fn pi11_gap_missing(id in arb_incident_id()) {
        let mut corpus = IncidentCorpus::new();
        corpus.register_gap(&id);
        prop_assert_eq!(corpus.coverage(&id), IncidentCoverageStatus::Missing);
    }

    // ── PI-12: Register overwrites gap ──────────────────────────────────────

    #[test]
    fn pi12_register_overwrites_gap(id in arb_incident_id()) {
        let mut corpus = IncidentCorpus::new();
        corpus.register_gap(&id);
        prop_assert_eq!(corpus.coverage(&id), IncidentCoverageStatus::Missing);
        corpus.register(&id, "/a.ftreplay", "bead-1");
        prop_assert_eq!(corpus.coverage(&id), IncidentCoverageStatus::Covered);
    }

    // ── PI-13: Unknown is Missing ───────────────────────────────────────────

    #[test]
    fn pi13_unknown_missing(id in arb_incident_id()) {
        let corpus = IncidentCorpus::new();
        prop_assert_eq!(corpus.coverage(&id), IncidentCoverageStatus::Missing);
    }

    // ── PI-14: Gaps count matches Missing entries ───────────────────────────

    #[test]
    fn pi14_gaps_count(
        covered_count in 0usize..5,
        gap_count in 0usize..5,
    ) {
        let mut corpus = IncidentCorpus::new();
        for i in 0..covered_count {
            corpus.register(&format!("COV-{}", i), "/a.ftreplay", &format!("b-{}", i));
        }
        for i in 0..gap_count {
            corpus.register_gap(&format!("GAP-{}", i));
        }
        prop_assert_eq!(corpus.gaps().len(), gap_count);
    }

    // ── PI-15: Coverage percent in [0, 100] ─────────────────────────────────

    #[test]
    fn pi15_coverage_bounded(
        covered_count in 0usize..10,
        gap_count in 0usize..10,
    ) {
        let mut corpus = IncidentCorpus::new();
        for i in 0..covered_count {
            corpus.register(&format!("COV-{}", i), "/a.ftreplay", &format!("b-{}", i));
        }
        for i in 0..gap_count {
            corpus.register_gap(&format!("GAP-{}", i));
        }
        let pct = corpus.coverage_percent();
        prop_assert!(pct >= 0.0);
        prop_assert!(pct <= 100.0);
    }

    // ── PI-16: Empty corpus has 100% coverage ───────────────────────────────

    #[test]
    fn pi16_empty_100(_dummy in 0u8..1) {
        let corpus = IncidentCorpus::new();
        let pct = corpus.coverage_percent();
        prop_assert!((pct - 100.0).abs() < f64::EPSILON);
    }

    // ── PI-17: PostIncidentInput serde roundtrip ────────────────────────────

    #[test]
    fn pi17_input_serde(input in arb_valid_input()) {
        let json = serde_json::to_string(&input).unwrap();
        let restored: PostIncidentInput = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, input);
    }

    // ── PI-18: PipelineResult serde roundtrip ───────────────────────────────

    #[test]
    fn pi18_result_serde(input in arb_valid_input()) {
        let result = execute_pipeline(&input);
        let json = serde_json::to_string(&result).unwrap();
        let restored: PipelineResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── PI-19: IncidentCorpus serde roundtrip ───────────────────────────────

    #[test]
    fn pi19_corpus_serde(count in 1usize..5) {
        let mut corpus = IncidentCorpus::new();
        for i in 0..count {
            corpus.register(&format!("INC-{}", i), "/a.ftreplay", &format!("b-{}", i));
        }
        let json = serde_json::to_string(&corpus).unwrap();
        let restored: IncidentCorpus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, corpus);
    }

    // ── PI-20: Report gap IDs are subset of entries ─────────────────────────

    #[test]
    fn pi20_gap_ids_subset(
        covered_count in 0usize..5,
        gap_count in 0usize..5,
    ) {
        let mut corpus = IncidentCorpus::new();
        for i in 0..covered_count {
            corpus.register(&format!("COV-{}", i), "/a.ftreplay", &format!("b-{}", i));
        }
        for i in 0..gap_count {
            corpus.register_gap(&format!("GAP-{}", i));
        }
        let report = corpus.coverage_report();
        for gap_id in &report.gap_incident_ids {
            let in_entries = corpus.entries.contains_key(gap_id);
            prop_assert!(in_entries);
        }
    }
}
