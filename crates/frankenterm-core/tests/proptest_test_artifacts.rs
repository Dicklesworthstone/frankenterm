//! Property-based tests for the `test_artifacts` module.
//!
//! Covers:
//! - ArtifactRunOutcome / ArtifactKind / ArtifactFormat serde roundtrips
//! - ArtifactCorrelation / StageTimingMetrics / ArtifactEntry serde roundtrips
//! - TestArtifactManifest serde roundtrip and validation invariants
//! - Negative timing rejection, percentile monotonicity, SHA256 validation
//! - Correlation identity requirements, failure artifact requirements
//!
//! Bead: wa-1u90p.7.1

use proptest::prelude::*;

use frankenterm_core::test_artifacts::{
    ArtifactCorrelation, ArtifactEntry, ArtifactFormat, ArtifactKind, ArtifactRunOutcome,
    StageTimingMetrics, TEST_ARTIFACT_SCHEMA_VERSION, TestArtifactManifest,
    TestArtifactSchemaError,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_outcome() -> impl Strategy<Value = ArtifactRunOutcome> {
    prop_oneof![
        Just(ArtifactRunOutcome::Passed),
        Just(ArtifactRunOutcome::Failed),
        Just(ArtifactRunOutcome::Aborted),
    ]
}

fn arb_artifact_kind() -> impl Strategy<Value = ArtifactKind> {
    prop_oneof![
        Just(ArtifactKind::StructuredLog),
        Just(ArtifactKind::EventStream),
        Just(ArtifactKind::AuditExtract),
        Just(ArtifactKind::TraceBundle),
        Just(ArtifactKind::FrameHistogram),
        Just(ArtifactKind::FailureSignature),
        Just(ArtifactKind::Screenshot),
        Just(ArtifactKind::Flamegraph),
        Just(ArtifactKind::RawData),
        Just(ArtifactKind::Other),
    ]
}

fn arb_artifact_format() -> impl Strategy<Value = ArtifactFormat> {
    prop_oneof![
        Just(ArtifactFormat::Json),
        Just(ArtifactFormat::JsonLines),
        Just(ArtifactFormat::Text),
        Just(ArtifactFormat::Csv),
        Just(ArtifactFormat::Html),
        Just(ArtifactFormat::Svg),
        Just(ArtifactFormat::Png),
        Just(ArtifactFormat::Binary),
    ]
}

fn arb_valid_sha256() -> impl Strategy<Value = String> {
    "[0-9a-f]{64}"
}

fn arb_correlation_with_identity() -> impl Strategy<Value = ArtifactCorrelation> {
    (
        "[a-z_]{3,20}",
        proptest::option::of("[a-z0-9-]{3,15}"),
        proptest::option::of(0_u64..10000),
        proptest::option::of(0_u64..1000),
        proptest::option::of(0_u64..10000),
        proptest::option::of("[a-z_]{3,15}"),
        proptest::option::of(0_u64..10000),
    )
        .prop_filter(
            "at least one additional identity required",
            |(_, txn, pane, tab, seq, sched, frame)| {
                txn.is_some()
                    || pane.is_some()
                    || tab.is_some()
                    || seq.is_some()
                    || sched.is_some()
                    || frame.is_some()
            },
        )
        .prop_map(
            |(
                test_case_id,
                resize_transaction_id,
                pane_id,
                tab_id,
                sequence_no,
                scheduler_decision,
                frame_id,
            )| {
                ArtifactCorrelation {
                    test_case_id,
                    resize_transaction_id,
                    pane_id,
                    tab_id,
                    sequence_no,
                    scheduler_decision,
                    frame_id,
                }
            },
        )
}

fn arb_non_negative_timing() -> impl Strategy<Value = StageTimingMetrics> {
    (
        proptest::option::of(0.0_f64..1000.0),
        proptest::option::of(0.0_f64..1000.0),
        proptest::option::of(0.0_f64..1000.0),
        proptest::option::of(0.0_f64..1000.0),
    )
        .prop_flat_map(|(queue, reflow, render, present)| {
            // Generate monotonic percentiles: p50 <= p95 <= p99
            (
                Just(queue),
                Just(reflow),
                Just(render),
                Just(present),
                prop_oneof![
                    // All None
                    Just((None, None, None)),
                    // All present and monotonic
                    (0.0_f64..100.0, 0.0_f64..100.0, 0.0_f64..100.0).prop_map(|(a, b, c)| {
                        let mut vals: [f64; 3] = (a, b, c).into();
                        vals.sort_by(|x, y| x.partial_cmp(y).unwrap());
                        (Some(vals[0]), Some(vals[1]), Some(vals[2]))
                    }),
                ],
            )
        })
        .prop_map(
            |(queue, reflow, render, present, (p50, p95, p99))| StageTimingMetrics {
                queue_wait_ms: queue,
                reflow_ms: reflow,
                render_ms: render,
                present_ms: present,
                p50_ms: p50,
                p95_ms: p95,
                p99_ms: p99,
            },
        )
}

fn arb_valid_artifact_entry() -> impl Strategy<Value = ArtifactEntry> {
    (
        arb_artifact_kind(),
        arb_artifact_format(),
        "[a-z/]{3,30}\\.[a-z]{2,5}",
        proptest::option::of(0_u64..10_000_000),
        proptest::option::of(arb_valid_sha256()),
        any::<bool>(),
    )
        .prop_map(
            |(kind, format, path, bytes, sha256, redacted)| ArtifactEntry {
                kind,
                format,
                path,
                bytes,
                sha256,
                redacted,
            },
        )
}

/// Build a valid manifest for the given outcome.
fn arb_valid_manifest(outcome: ArtifactRunOutcome) -> impl Strategy<Value = TestArtifactManifest> {
    (
        "[a-z0-9-]{3,15}",
        0_u64..10_000_000_000,
        arb_correlation_with_identity(),
        arb_non_negative_timing(),
        prop::collection::vec(arb_valid_artifact_entry(), 1..5),
    )
        .prop_map(
            move |(run_id, generated_at_ms, correlation, timing, mut artifacts)| {
                // For non-Passed outcomes, ensure required failure artifact kinds
                if outcome != ArtifactRunOutcome::Passed {
                    let kinds: std::collections::HashSet<_> =
                        artifacts.iter().map(|a| a.kind).collect();
                    for required_kind in [
                        ArtifactKind::TraceBundle,
                        ArtifactKind::FrameHistogram,
                        ArtifactKind::FailureSignature,
                    ] {
                        if !kinds.contains(&required_kind) {
                            artifacts.push(ArtifactEntry {
                                kind: required_kind,
                                format: ArtifactFormat::Json,
                                path: format!("required/{:?}.json", required_kind),
                                bytes: None,
                                sha256: None,
                                redacted: false,
                            });
                        }
                    }
                }
                TestArtifactManifest {
                    schema_version: TEST_ARTIFACT_SCHEMA_VERSION.to_string(),
                    run_id,
                    generated_at_ms,
                    outcome,
                    correlation,
                    timing,
                    artifacts,
                }
            },
        )
}

// =============================================================================
// Enum serde roundtrips
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ArtifactRunOutcome serde roundtrip preserves the variant.
    #[test]
    fn prop_outcome_serde_roundtrip(outcome in arb_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: ArtifactRunOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, outcome);
    }

    /// ArtifactKind serde roundtrip preserves the variant.
    #[test]
    fn prop_kind_serde_roundtrip(kind in arb_artifact_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: ArtifactKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }

    /// ArtifactFormat serde roundtrip preserves the variant.
    #[test]
    fn prop_format_serde_roundtrip(format in arb_artifact_format()) {
        let json = serde_json::to_string(&format).unwrap();
        let back: ArtifactFormat = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, format);
    }
}

// =============================================================================
// Struct serde roundtrips
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// ArtifactCorrelation serde roundtrip preserves all fields.
    #[test]
    fn prop_correlation_serde_roundtrip(corr in arb_correlation_with_identity()) {
        let json = serde_json::to_string(&corr).unwrap();
        let back: ArtifactCorrelation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &corr);
    }

    /// StageTimingMetrics serde roundtrip preserves all fields (within float tolerance).
    #[test]
    fn prop_timing_serde_roundtrip(timing in arb_non_negative_timing()) {
        let json = serde_json::to_string(&timing).unwrap();
        let back: StageTimingMetrics = serde_json::from_str(&json).unwrap();
        // f64 loses precision in JSON roundtrip — use tolerance
        fn close(a: Option<f64>, b: Option<f64>) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(x), Some(y)) => (x - y).abs() < 1e-10,
                _ => false,
            }
        }
        prop_assert!(close(back.queue_wait_ms, timing.queue_wait_ms), "queue_wait_ms mismatch");
        prop_assert!(close(back.reflow_ms, timing.reflow_ms), "reflow_ms mismatch");
        prop_assert!(close(back.render_ms, timing.render_ms), "render_ms mismatch");
        prop_assert!(close(back.present_ms, timing.present_ms), "present_ms mismatch");
        prop_assert!(close(back.p50_ms, timing.p50_ms), "p50_ms mismatch");
        prop_assert!(close(back.p95_ms, timing.p95_ms), "p95_ms mismatch");
        prop_assert!(close(back.p99_ms, timing.p99_ms), "p99_ms mismatch");
    }

    /// ArtifactEntry serde roundtrip preserves all fields.
    #[test]
    fn prop_entry_serde_roundtrip(entry in arb_valid_artifact_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: ArtifactEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back, &entry);
    }
}

// =============================================================================
// TestArtifactManifest serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Manifest serde roundtrip for Passed outcome.
    #[test]
    fn prop_manifest_serde_roundtrip_passed(manifest in arb_valid_manifest(ArtifactRunOutcome::Passed)) {
        let json = serde_json::to_string(&manifest).unwrap();
        let back: TestArtifactManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.schema_version, &manifest.schema_version);
        prop_assert_eq!(&back.run_id, &manifest.run_id);
        prop_assert_eq!(back.generated_at_ms, manifest.generated_at_ms);
        prop_assert_eq!(back.outcome, manifest.outcome);
        prop_assert_eq!(&back.correlation, &manifest.correlation);
        prop_assert_eq!(back.artifacts.len(), manifest.artifacts.len());
    }

    /// Manifest serde roundtrip for Failed outcome.
    #[test]
    fn prop_manifest_serde_roundtrip_failed(manifest in arb_valid_manifest(ArtifactRunOutcome::Failed)) {
        let json = serde_json::to_string(&manifest).unwrap();
        let back: TestArtifactManifest = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.outcome, ArtifactRunOutcome::Failed);
        prop_assert_eq!(&back.run_id, &manifest.run_id);
    }

    /// Manifest serde is deterministic.
    #[test]
    fn prop_manifest_serde_deterministic(manifest in arb_valid_manifest(ArtifactRunOutcome::Passed)) {
        let j1 = serde_json::to_string(&manifest).unwrap();
        let j2 = serde_json::to_string(&manifest).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =============================================================================
// Validate — valid manifests pass
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Well-formed Passed manifests always pass validation.
    #[test]
    fn prop_valid_passed_manifest_validates(manifest in arb_valid_manifest(ArtifactRunOutcome::Passed)) {
        prop_assert!(
            manifest.validate().is_ok(),
            "valid Passed manifest should validate"
        );
    }

    /// Well-formed Failed manifests always pass validation.
    #[test]
    fn prop_valid_failed_manifest_validates(manifest in arb_valid_manifest(ArtifactRunOutcome::Failed)) {
        prop_assert!(
            manifest.validate().is_ok(),
            "valid Failed manifest should validate"
        );
    }

    /// Well-formed Aborted manifests always pass validation.
    #[test]
    fn prop_valid_aborted_manifest_validates(manifest in arb_valid_manifest(ArtifactRunOutcome::Aborted)) {
        prop_assert!(
            manifest.validate().is_ok(),
            "valid Aborted manifest should validate"
        );
    }
}

// =============================================================================
// Validate — schema version
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Wrong schema version is always rejected.
    #[test]
    fn prop_wrong_schema_version_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        bad_version in "[a-z.]{1,20}",
    ) {
        prop_assume!(bad_version != TEST_ARTIFACT_SCHEMA_VERSION);
        let mut m = manifest;
        m.schema_version = bad_version.clone();
        let err = m.validate().expect_err("wrong schema version should fail");
        match err {
            TestArtifactSchemaError::InvalidSchemaVersion { found } => {
                prop_assert_eq!(&found, &bad_version);
            }
            other => prop_assert!(false, "expected InvalidSchemaVersion, got {:?}", other),
        }
    }
}

// =============================================================================
// Validate — run_id and test_case_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Empty or whitespace-only run_id is rejected.
    #[test]
    fn prop_empty_run_id_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        whitespace in "[ \t\n]{0,5}",
    ) {
        let mut m = manifest;
        m.run_id = whitespace;
        let err = m.validate().expect_err("empty run_id should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::MissingRunId),
            "expected MissingRunId, got {:?}", err
        );
    }

    /// Empty or whitespace-only test_case_id is rejected.
    #[test]
    fn prop_empty_test_case_id_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        whitespace in "[ \t\n]{0,5}",
    ) {
        let mut m = manifest;
        m.correlation.test_case_id = whitespace;
        let err = m.validate().expect_err("empty test_case_id should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::MissingTestCaseId),
            "expected MissingTestCaseId, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — correlation identity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Missing all optional correlation fields is rejected.
    #[test]
    fn prop_missing_correlation_identity_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
    ) {
        let mut m = manifest;
        m.correlation.resize_transaction_id = None;
        m.correlation.pane_id = None;
        m.correlation.tab_id = None;
        m.correlation.sequence_no = None;
        m.correlation.scheduler_decision = None;
        m.correlation.frame_id = None;
        let err = m.validate().expect_err("missing identity should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::MissingCorrelationIdentity),
            "expected MissingCorrelationIdentity, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — empty artifacts
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Empty artifact list is rejected.
    #[test]
    fn prop_empty_artifacts_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
    ) {
        let mut m = manifest;
        m.artifacts.clear();
        let err = m.validate().expect_err("empty artifacts should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::MissingArtifacts),
            "expected MissingArtifacts, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — negative timing rejection
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Any negative timing value is rejected.
    #[test]
    fn prop_negative_timing_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        neg_value in -1000.0_f64..-0.001,
        field_idx in 0_usize..7,
    ) {
        let mut m = manifest;
        // Clear percentiles to avoid order check interfering
        m.timing.p50_ms = None;
        m.timing.p95_ms = None;
        m.timing.p99_ms = None;

        match field_idx {
            0 => m.timing.queue_wait_ms = Some(neg_value),
            1 => m.timing.reflow_ms = Some(neg_value),
            2 => m.timing.render_ms = Some(neg_value),
            3 => m.timing.present_ms = Some(neg_value),
            4 => {
                m.timing.p50_ms = Some(neg_value);
                // Don't set p95/p99 to avoid percentile ordering check
            }
            5 => {
                m.timing.p95_ms = Some(neg_value);
            }
            _ => {
                m.timing.p99_ms = Some(neg_value);
            }
        }

        let err = m.validate().expect_err("negative timing should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::NegativeTiming { .. }),
            "expected NegativeTiming, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — percentile ordering
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Monotonic percentiles (p50 <= p95 <= p99) pass validation.
    #[test]
    fn prop_monotonic_percentiles_pass(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        a in 0.0_f64..100.0,
        b in 0.0_f64..100.0,
        c in 0.0_f64..100.0,
    ) {
        let mut vals = [a, b, c];
        vals.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let mut m = manifest;
        m.timing.p50_ms = Some(vals[0]);
        m.timing.p95_ms = Some(vals[1]);
        m.timing.p99_ms = Some(vals[2]);
        // Ensure other timings are non-negative
        m.timing.queue_wait_ms = Some(0.0);
        m.timing.reflow_ms = Some(0.0);
        m.timing.render_ms = Some(0.0);
        m.timing.present_ms = Some(0.0);
        prop_assert!(m.validate().is_ok(), "monotonic percentiles should pass");
    }

    /// Non-monotonic percentiles are rejected.
    #[test]
    fn prop_non_monotonic_percentiles_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        p50 in 0.1_f64..100.0,
        p95 in 0.1_f64..100.0,
        p99 in 0.1_f64..100.0,
    ) {
        // Only test cases where ordering is actually violated
        prop_assume!(!(p50 <= p95 && p95 <= p99));

        let mut m = manifest;
        m.timing.p50_ms = Some(p50);
        m.timing.p95_ms = Some(p95);
        m.timing.p99_ms = Some(p99);
        m.timing.queue_wait_ms = Some(0.0);
        m.timing.reflow_ms = Some(0.0);
        m.timing.render_ms = Some(0.0);
        m.timing.present_ms = Some(0.0);
        let err = m.validate().expect_err("non-monotonic percentiles should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::InvalidPercentileOrder { .. }),
            "expected InvalidPercentileOrder, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — SHA256
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Valid 64-char hex SHA256 passes validation.
    #[test]
    fn prop_valid_sha256_passes(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        hash in arb_valid_sha256(),
    ) {
        let mut m = manifest;
        m.artifacts[0].sha256 = Some(hash);
        prop_assert!(m.validate().is_ok(), "valid sha256 should pass");
    }

    /// SHA256 with wrong length is rejected.
    #[test]
    fn prop_sha256_wrong_length_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        len in 1_usize..63,
    ) {
        let mut m = manifest;
        let bad_hash = "a".repeat(len);
        m.artifacts[0].sha256 = Some(bad_hash);
        let err = m.validate().expect_err("wrong-length sha256 should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::InvalidSha256 { .. }),
            "expected InvalidSha256, got {:?}", err
        );
    }

    /// SHA256 with non-hex characters is rejected.
    #[test]
    fn prop_sha256_non_hex_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        bad_char in "[g-z]",
    ) {
        let mut m = manifest;
        let mut hash = "a".repeat(63);
        hash.push_str(&bad_char);
        m.artifacts[0].sha256 = Some(hash);
        let err = m.validate().expect_err("non-hex sha256 should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::InvalidSha256 { .. }),
            "expected InvalidSha256, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — empty artifact path
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Empty or whitespace-only artifact path is rejected.
    #[test]
    fn prop_empty_artifact_path_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
        whitespace in "[ \t]{0,5}",
    ) {
        let mut m = manifest;
        m.artifacts[0].path = whitespace;
        let err = m.validate().expect_err("empty path should fail");
        prop_assert!(
            matches!(err, TestArtifactSchemaError::MissingArtifactPath { index: 0 }),
            "expected MissingArtifactPath at index 0, got {:?}", err
        );
    }
}

// =============================================================================
// Validate — failure artifact requirements
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Failed manifest missing TraceBundle is rejected.
    #[test]
    fn prop_failed_missing_trace_bundle_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Failed),
    ) {
        let mut m = manifest;
        m.artifacts.retain(|a| a.kind != ArtifactKind::TraceBundle);
        let err = m.validate().expect_err("missing TraceBundle should fail");
        prop_assert!(
            matches!(
                err,
                TestArtifactSchemaError::MissingRequiredArtifactKind {
                    kind: ArtifactKind::TraceBundle
                }
            ),
            "expected MissingRequiredArtifactKind(TraceBundle), got {:?}", err
        );
    }

    /// Failed manifest missing FrameHistogram is rejected.
    #[test]
    fn prop_failed_missing_frame_histogram_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Failed),
    ) {
        let mut m = manifest;
        m.artifacts.retain(|a| a.kind != ArtifactKind::FrameHistogram);
        let err = m.validate().expect_err("missing FrameHistogram should fail");
        prop_assert!(
            matches!(
                err,
                TestArtifactSchemaError::MissingRequiredArtifactKind {
                    kind: ArtifactKind::FrameHistogram
                }
            ),
            "expected MissingRequiredArtifactKind(FrameHistogram), got {:?}", err
        );
    }

    /// Failed manifest missing FailureSignature is rejected.
    #[test]
    fn prop_failed_missing_failure_signature_rejected(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Failed),
    ) {
        let mut m = manifest;
        m.artifacts.retain(|a| a.kind != ArtifactKind::FailureSignature);
        let err = m.validate().expect_err("missing FailureSignature should fail");
        prop_assert!(
            matches!(
                err,
                TestArtifactSchemaError::MissingRequiredArtifactKind {
                    kind: ArtifactKind::FailureSignature
                }
            ),
            "expected MissingRequiredArtifactKind(FailureSignature), got {:?}", err
        );
    }

    /// Passed manifests don't require failure artifacts — any artifact kind suffices.
    #[test]
    fn prop_passed_no_failure_artifacts_needed(
        manifest in arb_valid_manifest(ArtifactRunOutcome::Passed),
    ) {
        let mut m = manifest;
        // Remove all failure-related kinds if present
        m.artifacts.retain(|a| {
            !matches!(
                a.kind,
                ArtifactKind::TraceBundle | ArtifactKind::FrameHistogram | ArtifactKind::FailureSignature
            )
        });
        // Ensure at least one artifact remains
        if m.artifacts.is_empty() {
            m.artifacts.push(ArtifactEntry {
                kind: ArtifactKind::StructuredLog,
                format: ArtifactFormat::JsonLines,
                path: "logs/test.jsonl".to_string(),
                bytes: None,
                sha256: None,
                redacted: false,
            });
        }
        prop_assert!(m.validate().is_ok(), "Passed manifest should not need failure artifacts");
    }
}

// =============================================================================
// Display impl for TestArtifactSchemaError
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// TestArtifactSchemaError Display produces non-empty output.
    #[test]
    fn prop_schema_error_display_non_empty(
        version in "[a-z.]{1,10}",
    ) {
        let errors = vec![
            TestArtifactSchemaError::InvalidSchemaVersion { found: version },
            TestArtifactSchemaError::MissingRunId,
            TestArtifactSchemaError::MissingTestCaseId,
            TestArtifactSchemaError::MissingCorrelationIdentity,
            TestArtifactSchemaError::MissingArtifacts,
            TestArtifactSchemaError::MissingArtifactPath { index: 0 },
            TestArtifactSchemaError::MissingRequiredArtifactKind { kind: ArtifactKind::TraceBundle },
            TestArtifactSchemaError::NegativeTiming { field: "p50_ms", value: -1.0 },
            TestArtifactSchemaError::InvalidPercentileOrder { p50: 5.0, p95: 3.0, p99: 7.0 },
            TestArtifactSchemaError::InvalidSha256 { index: 0, value: "bad".to_string() },
        ];

        for err in &errors {
            let display = err.to_string();
            prop_assert!(!display.is_empty(), "Display for {:?} should not be empty", err);
        }
    }
}

// =============================================================================
// Unit tests (supplementary)
// =============================================================================

#[test]
fn default_timing_is_all_none() {
    let timing = StageTimingMetrics::default();
    assert!(timing.queue_wait_ms.is_none());
    assert!(timing.reflow_ms.is_none());
    assert!(timing.render_ms.is_none());
    assert!(timing.present_ms.is_none());
    assert!(timing.p50_ms.is_none());
    assert!(timing.p95_ms.is_none());
    assert!(timing.p99_ms.is_none());
}

#[test]
fn schema_version_constant_is_stable() {
    assert_eq!(TEST_ARTIFACT_SCHEMA_VERSION, "wa.test_artifacts.v1");
}

#[test]
fn all_outcomes_distinct() {
    assert_ne!(ArtifactRunOutcome::Passed, ArtifactRunOutcome::Failed);
    assert_ne!(ArtifactRunOutcome::Passed, ArtifactRunOutcome::Aborted);
    assert_ne!(ArtifactRunOutcome::Failed, ArtifactRunOutcome::Aborted);
}
