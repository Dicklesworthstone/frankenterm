//! Canonical test artifact schema for resize/reflow validation outputs.
//!
//! This module provides a machine-parseable contract for test artifact bundles
//! so CI, dashboards, and triage tooling can rely on one stable structure.

use serde::{Deserialize, Serialize};

/// Stable schema version identifier for test artifact manifests.
pub const TEST_ARTIFACT_SCHEMA_VERSION: &str = "wa.test_artifacts.v1";

/// Result category for a test run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRunOutcome {
    Passed,
    Failed,
    Aborted,
}

/// Correlation identifiers that connect artifacts to resize transactions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCorrelation {
    /// Stable test-case identifier (required).
    pub test_case_id: String,
    /// Resize transaction identifier, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resize_transaction_id: Option<String>,
    /// Pane identifier, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    /// Tab identifier, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<u64>,
    /// Sequence number, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_no: Option<u64>,
    /// Scheduler decision label, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler_decision: Option<String>,
    /// Frame identifier, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<u64>,
}

impl ArtifactCorrelation {
    fn has_additional_identity(&self) -> bool {
        self.resize_transaction_id.is_some()
            || self.pane_id.is_some()
            || self.tab_id.is_some()
            || self.sequence_no.is_some()
            || self.scheduler_decision.is_some()
            || self.frame_id.is_some()
    }
}

/// Stage timing metrics associated with a test run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StageTimingMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflow_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_ms: Option<f64>,
}

/// Artifact kind classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    StructuredLog,
    EventStream,
    AuditExtract,
    TraceBundle,
    FrameHistogram,
    FailureSignature,
    Screenshot,
    Flamegraph,
    RawData,
    Other,
}

/// On-disk data format for a single artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactFormat {
    Json,
    JsonLines,
    Text,
    Csv,
    Html,
    Svg,
    Png,
    Binary,
}

/// Single artifact entry in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactEntry {
    pub kind: ArtifactKind,
    pub format: ArtifactFormat,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether secret redaction has already been applied.
    #[serde(default)]
    pub redacted: bool,
}

/// Manifest that defines a complete test artifact bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestArtifactManifest {
    pub schema_version: String,
    pub run_id: String,
    pub generated_at_ms: u64,
    pub outcome: ArtifactRunOutcome,
    pub correlation: ArtifactCorrelation,
    #[serde(default)]
    pub timing: StageTimingMetrics,
    pub artifacts: Vec<ArtifactEntry>,
}

impl TestArtifactManifest {
    /// Validate the manifest contract.
    pub fn validate(&self) -> Result<(), TestArtifactSchemaError> {
        if self.schema_version != TEST_ARTIFACT_SCHEMA_VERSION {
            return Err(TestArtifactSchemaError::InvalidSchemaVersion {
                found: self.schema_version.clone(),
            });
        }
        if self.run_id.trim().is_empty() {
            return Err(TestArtifactSchemaError::MissingRunId);
        }
        if self.correlation.test_case_id.trim().is_empty() {
            return Err(TestArtifactSchemaError::MissingTestCaseId);
        }
        if !self.correlation.has_additional_identity() {
            return Err(TestArtifactSchemaError::MissingCorrelationIdentity);
        }
        if self.artifacts.is_empty() {
            return Err(TestArtifactSchemaError::MissingArtifacts);
        }

        self.validate_timings()?;
        self.validate_artifacts()?;

        Ok(())
    }

    fn validate_timings(&self) -> Result<(), TestArtifactSchemaError> {
        for (name, value) in [
            ("queue_wait_ms", self.timing.queue_wait_ms),
            ("reflow_ms", self.timing.reflow_ms),
            ("render_ms", self.timing.render_ms),
            ("present_ms", self.timing.present_ms),
            ("p50_ms", self.timing.p50_ms),
            ("p95_ms", self.timing.p95_ms),
            ("p99_ms", self.timing.p99_ms),
        ] {
            if let Some(v) = value {
                if v.is_sign_negative() {
                    return Err(TestArtifactSchemaError::NegativeTiming {
                        field: name,
                        value: v,
                    });
                }
            }
        }

        if let (Some(p50), Some(p95), Some(p99)) =
            (self.timing.p50_ms, self.timing.p95_ms, self.timing.p99_ms)
        {
            if !(p50 <= p95 && p95 <= p99) {
                return Err(TestArtifactSchemaError::InvalidPercentileOrder { p50, p95, p99 });
            }
        }

        Ok(())
    }

    fn validate_artifacts(&self) -> Result<(), TestArtifactSchemaError> {
        let mut kinds = std::collections::HashSet::new();

        for (idx, artifact) in self.artifacts.iter().enumerate() {
            if artifact.path.trim().is_empty() {
                return Err(TestArtifactSchemaError::MissingArtifactPath { index: idx });
            }
            if let Some(hash) = &artifact.sha256 {
                let valid = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
                if !valid {
                    return Err(TestArtifactSchemaError::InvalidSha256 {
                        index: idx,
                        value: hash.clone(),
                    });
                }
            }
            kinds.insert(artifact.kind);
        }

        if self.outcome != ArtifactRunOutcome::Passed {
            for required in [
                ArtifactKind::TraceBundle,
                ArtifactKind::FrameHistogram,
                ArtifactKind::FailureSignature,
            ] {
                if !kinds.contains(&required) {
                    return Err(TestArtifactSchemaError::MissingRequiredArtifactKind {
                        kind: required,
                    });
                }
            }
        }

        Ok(())
    }
}

/// Validation errors for [`TestArtifactManifest`].
#[derive(Debug, Clone, PartialEq)]
pub enum TestArtifactSchemaError {
    InvalidSchemaVersion { found: String },
    MissingRunId,
    MissingTestCaseId,
    MissingCorrelationIdentity,
    MissingArtifacts,
    MissingArtifactPath { index: usize },
    MissingRequiredArtifactKind { kind: ArtifactKind },
    NegativeTiming { field: &'static str, value: f64 },
    InvalidPercentileOrder { p50: f64, p95: f64, p99: f64 },
    InvalidSha256 { index: usize, value: String },
}

impl std::fmt::Display for TestArtifactSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchemaVersion { found } => {
                write!(f, "invalid schema version: {found}")
            }
            Self::MissingRunId => write!(f, "run_id is required"),
            Self::MissingTestCaseId => write!(f, "correlation.test_case_id is required"),
            Self::MissingCorrelationIdentity => write!(
                f,
                "at least one correlation identity beyond test_case_id is required"
            ),
            Self::MissingArtifacts => write!(f, "at least one artifact entry is required"),
            Self::MissingArtifactPath { index } => {
                write!(f, "artifact at index {index} has empty path")
            }
            Self::MissingRequiredArtifactKind { kind } => {
                write!(f, "missing required artifact kind: {kind:?}")
            }
            Self::NegativeTiming { field, value } => {
                write!(f, "timing field {field} must be non-negative (got {value})")
            }
            Self::InvalidPercentileOrder { p50, p95, p99 } => write!(
                f,
                "invalid percentile ordering: expected p50 <= p95 <= p99, got {p50}, {p95}, {p99}"
            ),
            Self::InvalidSha256 { index, value } => {
                write!(f, "artifact at index {index} has invalid sha256 '{value}'")
            }
        }
    }
}

impl std::error::Error for TestArtifactSchemaError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest(outcome: ArtifactRunOutcome) -> TestArtifactManifest {
        let mut artifacts = vec![ArtifactEntry {
            kind: ArtifactKind::StructuredLog,
            format: ArtifactFormat::JsonLines,
            path: "logs/resize.jsonl".to_string(),
            bytes: Some(123),
            sha256: Some("a".repeat(64)),
            redacted: true,
        }];

        if outcome != ArtifactRunOutcome::Passed {
            artifacts.push(ArtifactEntry {
                kind: ArtifactKind::TraceBundle,
                format: ArtifactFormat::Json,
                path: "traces/trace_bundle.json".to_string(),
                bytes: Some(22),
                sha256: None,
                redacted: true,
            });
            artifacts.push(ArtifactEntry {
                kind: ArtifactKind::FrameHistogram,
                format: ArtifactFormat::Json,
                path: "metrics/frame_histogram.json".to_string(),
                bytes: Some(33),
                sha256: None,
                redacted: true,
            });
            artifacts.push(ArtifactEntry {
                kind: ArtifactKind::FailureSignature,
                format: ArtifactFormat::Text,
                path: "failure/signature.txt".to_string(),
                bytes: Some(44),
                sha256: None,
                redacted: true,
            });
        }

        TestArtifactManifest {
            schema_version: TEST_ARTIFACT_SCHEMA_VERSION.to_string(),
            run_id: "run-123".to_string(),
            generated_at_ms: 1_735_000_000_000,
            outcome,
            correlation: ArtifactCorrelation {
                test_case_id: "resize_storm_01".to_string(),
                resize_transaction_id: Some("txn-42".to_string()),
                pane_id: Some(1),
                tab_id: Some(7),
                sequence_no: Some(9),
                scheduler_decision: Some("fair_share".to_string()),
                frame_id: Some(10),
            },
            timing: StageTimingMetrics {
                queue_wait_ms: Some(1.0),
                reflow_ms: Some(2.0),
                render_ms: Some(3.0),
                present_ms: Some(4.0),
                p50_ms: Some(2.0),
                p95_ms: Some(4.0),
                p99_ms: Some(5.0),
            },
            artifacts,
        }
    }

    #[test]
    fn valid_failed_manifest_passes_validation() {
        let manifest = valid_manifest(ArtifactRunOutcome::Failed);
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn failed_manifest_requires_failure_artifacts() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Failed);
        manifest
            .artifacts
            .retain(|a| a.kind != ArtifactKind::FailureSignature);

        let err = manifest.validate().expect_err("validation should fail");
        assert!(matches!(
            err,
            TestArtifactSchemaError::MissingRequiredArtifactKind {
                kind: ArtifactKind::FailureSignature
            }
        ));
    }

    #[test]
    fn percentile_order_must_be_monotonic() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.p50_ms = Some(5.0);
        manifest.timing.p95_ms = Some(4.0);
        manifest.timing.p99_ms = Some(6.0);

        let err = manifest.validate().expect_err("validation should fail");
        assert!(matches!(
            err,
            TestArtifactSchemaError::InvalidPercentileOrder { .. }
        ));
    }

    #[test]
    fn invalid_sha256_is_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts[0].sha256 = Some("xyz".to_string());

        let err = manifest.validate().expect_err("validation should fail");
        assert!(matches!(err, TestArtifactSchemaError::InvalidSha256 { .. }));
    }

    #[test]
    fn missing_correlation_identity_is_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.correlation.resize_transaction_id = None;
        manifest.correlation.pane_id = None;
        manifest.correlation.tab_id = None;
        manifest.correlation.sequence_no = None;
        manifest.correlation.scheduler_decision = None;
        manifest.correlation.frame_id = None;

        let err = manifest.validate().expect_err("validation should fail");
        assert!(matches!(
            err,
            TestArtifactSchemaError::MissingCorrelationIdentity
        ));
    }

    // =====================================================================
    // Validation edge cases
    // =====================================================================

    #[test]
    fn valid_passed_manifest_passes_validation() {
        let manifest = valid_manifest(ArtifactRunOutcome::Passed);
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn valid_aborted_manifest_passes_validation() {
        let manifest = valid_manifest(ArtifactRunOutcome::Aborted);
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn invalid_schema_version_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.schema_version = "v0.bad".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::InvalidSchemaVersion { .. }
        ));
        assert!(err.to_string().contains("v0.bad"));
    }

    #[test]
    fn empty_run_id_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.run_id = "   ".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, TestArtifactSchemaError::MissingRunId));
    }

    #[test]
    fn empty_test_case_id_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.correlation.test_case_id = "".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, TestArtifactSchemaError::MissingTestCaseId));
    }

    #[test]
    fn no_artifacts_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts.clear();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, TestArtifactSchemaError::MissingArtifacts));
    }

    #[test]
    fn empty_artifact_path_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts[0].path = "  ".to_string();
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::MissingArtifactPath { index: 0 }
        ));
    }

    #[test]
    fn negative_timing_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.reflow_ms = Some(-1.0);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::NegativeTiming { field: "reflow_ms", .. }
        ));
    }

    #[test]
    fn negative_queue_wait_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.queue_wait_ms = Some(-0.001);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::NegativeTiming { field: "queue_wait_ms", .. }
        ));
    }

    #[test]
    fn zero_timings_accepted() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.queue_wait_ms = Some(0.0);
        manifest.timing.reflow_ms = Some(0.0);
        manifest.timing.p50_ms = Some(0.0);
        manifest.timing.p95_ms = Some(0.0);
        manifest.timing.p99_ms = Some(0.0);
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn percentile_order_equal_values_accepted() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.p50_ms = Some(5.0);
        manifest.timing.p95_ms = Some(5.0);
        manifest.timing.p99_ms = Some(5.0);
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn partial_percentiles_skip_order_check() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.p50_ms = Some(100.0);
        manifest.timing.p95_ms = None; // Missing p95 → skip ordering check
        manifest.timing.p99_ms = Some(1.0);
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn aborted_manifest_missing_trace_bundle_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Aborted);
        manifest
            .artifacts
            .retain(|a| a.kind != ArtifactKind::TraceBundle);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::MissingRequiredArtifactKind {
                kind: ArtifactKind::TraceBundle
            }
        ));
    }

    #[test]
    fn aborted_manifest_missing_frame_histogram_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Aborted);
        manifest
            .artifacts
            .retain(|a| a.kind != ArtifactKind::FrameHistogram);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::MissingRequiredArtifactKind {
                kind: ArtifactKind::FrameHistogram
            }
        ));
    }

    #[test]
    fn passed_manifest_no_failure_artifacts_ok() {
        // Passed outcome doesn't require TraceBundle/FrameHistogram/FailureSignature
        let manifest = valid_manifest(ArtifactRunOutcome::Passed);
        assert_eq!(manifest.artifacts.len(), 1); // Only StructuredLog
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn sha256_wrong_length_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts[0].sha256 = Some("abcdef".to_string()); // Too short
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, TestArtifactSchemaError::InvalidSha256 { .. }));
    }

    #[test]
    fn sha256_non_hex_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts[0].sha256 = Some("g".repeat(64)); // Non-hex chars
        let err = manifest.validate().unwrap_err();
        assert!(matches!(err, TestArtifactSchemaError::InvalidSha256 { .. }));
    }

    #[test]
    fn sha256_none_accepted() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts[0].sha256 = None;
        assert!(manifest.validate().is_ok());
    }

    #[test]
    fn sha256_valid_hex_accepted() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.artifacts[0].sha256 =
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string());
        assert!(manifest.validate().is_ok());
    }

    // =====================================================================
    // ArtifactCorrelation has_additional_identity
    // =====================================================================

    #[test]
    fn correlation_no_additional_identity() {
        let c = ArtifactCorrelation {
            test_case_id: "test1".to_string(),
            resize_transaction_id: None,
            pane_id: None,
            tab_id: None,
            sequence_no: None,
            scheduler_decision: None,
            frame_id: None,
        };
        assert!(!c.has_additional_identity());
    }

    #[test]
    fn correlation_each_field_counts_as_identity() {
        let base = ArtifactCorrelation {
            test_case_id: "t".to_string(),
            resize_transaction_id: None,
            pane_id: None,
            tab_id: None,
            sequence_no: None,
            scheduler_decision: None,
            frame_id: None,
        };

        let mut c = base.clone();
        c.resize_transaction_id = Some("tx".to_string());
        assert!(c.has_additional_identity());

        let mut c = base.clone();
        c.pane_id = Some(1);
        assert!(c.has_additional_identity());

        let mut c = base.clone();
        c.tab_id = Some(1);
        assert!(c.has_additional_identity());

        let mut c = base.clone();
        c.sequence_no = Some(0);
        assert!(c.has_additional_identity());

        let mut c = base.clone();
        c.scheduler_decision = Some("round_robin".to_string());
        assert!(c.has_additional_identity());

        let mut c = base;
        c.frame_id = Some(99);
        assert!(c.has_additional_identity());
    }

    // =====================================================================
    // Serde roundtrips
    // =====================================================================

    #[test]
    fn manifest_serde_roundtrip() {
        let manifest = valid_manifest(ArtifactRunOutcome::Failed);
        let json = serde_json::to_string(&manifest).unwrap();
        let deserialized: TestArtifactManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, deserialized);
    }

    #[test]
    fn artifact_run_outcome_serde() {
        for outcome in [
            ArtifactRunOutcome::Passed,
            ArtifactRunOutcome::Failed,
            ArtifactRunOutcome::Aborted,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let de: ArtifactRunOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(outcome, de);
        }
    }

    #[test]
    fn artifact_kind_serde_all_variants() {
        let kinds = [
            ArtifactKind::StructuredLog,
            ArtifactKind::EventStream,
            ArtifactKind::AuditExtract,
            ArtifactKind::TraceBundle,
            ArtifactKind::FrameHistogram,
            ArtifactKind::FailureSignature,
            ArtifactKind::Screenshot,
            ArtifactKind::Flamegraph,
            ArtifactKind::RawData,
            ArtifactKind::Other,
        ];
        for kind in kinds {
            let json = serde_json::to_string(&kind).unwrap();
            let de: ArtifactKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, de);
        }
    }

    #[test]
    fn artifact_format_serde_all_variants() {
        let formats = [
            ArtifactFormat::Json,
            ArtifactFormat::JsonLines,
            ArtifactFormat::Text,
            ArtifactFormat::Csv,
            ArtifactFormat::Html,
            ArtifactFormat::Svg,
            ArtifactFormat::Png,
            ArtifactFormat::Binary,
        ];
        for fmt in formats {
            let json = serde_json::to_string(&fmt).unwrap();
            let de: ArtifactFormat = serde_json::from_str(&json).unwrap();
            assert_eq!(fmt, de);
        }
    }

    #[test]
    fn artifact_run_outcome_snake_case_serde() {
        assert_eq!(
            serde_json::to_string(&ArtifactRunOutcome::Passed).unwrap(),
            "\"passed\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactRunOutcome::Failed).unwrap(),
            "\"failed\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactRunOutcome::Aborted).unwrap(),
            "\"aborted\""
        );
    }

    #[test]
    fn artifact_kind_snake_case_serde() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::StructuredLog).unwrap(),
            "\"structured_log\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::FailureSignature).unwrap(),
            "\"failure_signature\""
        );
    }

    #[test]
    fn correlation_serde_skips_none_fields() {
        let c = ArtifactCorrelation {
            test_case_id: "t1".to_string(),
            resize_transaction_id: None,
            pane_id: Some(5),
            tab_id: None,
            sequence_no: None,
            scheduler_decision: None,
            frame_id: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("resize_transaction_id"));
        assert!(json.contains("pane_id"));
    }

    // =====================================================================
    // StageTimingMetrics tests
    // =====================================================================

    #[test]
    fn stage_timing_metrics_default_all_none() {
        let t = StageTimingMetrics::default();
        assert!(t.queue_wait_ms.is_none());
        assert!(t.reflow_ms.is_none());
        assert!(t.render_ms.is_none());
        assert!(t.present_ms.is_none());
        assert!(t.p50_ms.is_none());
        assert!(t.p95_ms.is_none());
        assert!(t.p99_ms.is_none());
    }

    #[test]
    fn stage_timing_metrics_clone() {
        let t = StageTimingMetrics {
            queue_wait_ms: Some(1.5),
            reflow_ms: Some(2.0),
            render_ms: None,
            present_ms: None,
            p50_ms: Some(3.0),
            p95_ms: Some(4.0),
            p99_ms: Some(5.0),
        };
        let t2 = t.clone();
        assert_eq!(t, t2);
    }

    // =====================================================================
    // ArtifactEntry tests
    // =====================================================================

    #[test]
    fn artifact_entry_redacted_default_false() {
        let json = r#"{"kind":"raw_data","format":"binary","path":"data.bin"}"#;
        let entry: ArtifactEntry = serde_json::from_str(json).unwrap();
        assert!(!entry.redacted);
        assert!(entry.bytes.is_none());
        assert!(entry.sha256.is_none());
    }

    #[test]
    fn artifact_entry_clone_eq() {
        let e = ArtifactEntry {
            kind: ArtifactKind::Screenshot,
            format: ArtifactFormat::Png,
            path: "screenshot.png".to_string(),
            bytes: Some(4096),
            sha256: None,
            redacted: false,
        };
        let e2 = e.clone();
        assert_eq!(e, e2);
    }

    // =====================================================================
    // TestArtifactSchemaError Display tests
    // =====================================================================

    #[test]
    fn schema_error_display_all_variants() {
        let cases: Vec<(TestArtifactSchemaError, &str)> = vec![
            (
                TestArtifactSchemaError::InvalidSchemaVersion {
                    found: "bad".into(),
                },
                "invalid schema version: bad",
            ),
            (TestArtifactSchemaError::MissingRunId, "run_id is required"),
            (
                TestArtifactSchemaError::MissingTestCaseId,
                "correlation.test_case_id is required",
            ),
            (
                TestArtifactSchemaError::MissingCorrelationIdentity,
                "at least one correlation identity",
            ),
            (
                TestArtifactSchemaError::MissingArtifacts,
                "at least one artifact entry",
            ),
            (
                TestArtifactSchemaError::MissingArtifactPath { index: 3 },
                "artifact at index 3",
            ),
            (
                TestArtifactSchemaError::MissingRequiredArtifactKind {
                    kind: ArtifactKind::TraceBundle,
                },
                "missing required artifact kind",
            ),
            (
                TestArtifactSchemaError::NegativeTiming {
                    field: "reflow_ms",
                    value: -1.0,
                },
                "must be non-negative",
            ),
            (
                TestArtifactSchemaError::InvalidPercentileOrder {
                    p50: 5.0,
                    p95: 3.0,
                    p99: 10.0,
                },
                "invalid percentile ordering",
            ),
            (
                TestArtifactSchemaError::InvalidSha256 {
                    index: 0,
                    value: "bad".into(),
                },
                "invalid sha256",
            ),
        ];
        for (err, expected_substr) in cases {
            let msg = err.to_string();
            assert!(
                msg.contains(expected_substr),
                "Expected '{}' to contain '{}'",
                msg,
                expected_substr
            );
        }
    }

    #[test]
    fn schema_error_is_std_error() {
        let err: Box<dyn std::error::Error> =
            Box::new(TestArtifactSchemaError::MissingRunId);
        assert!(err.to_string().contains("run_id"));
    }

    // =====================================================================
    // Schema version constant
    // =====================================================================

    #[test]
    fn schema_version_constant_is_stable() {
        assert_eq!(TEST_ARTIFACT_SCHEMA_VERSION, "wa.test_artifacts.v1");
    }

    // =====================================================================
    // Enum Hash trait usage
    // =====================================================================

    #[test]
    fn artifact_run_outcome_hash_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ArtifactRunOutcome::Passed);
        set.insert(ArtifactRunOutcome::Failed);
        set.insert(ArtifactRunOutcome::Aborted);
        assert_eq!(set.len(), 3);
        set.insert(ArtifactRunOutcome::Passed);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn artifact_kind_hash_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ArtifactKind::StructuredLog);
        set.insert(ArtifactKind::Other);
        set.insert(ArtifactKind::Flamegraph);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn artifact_format_hash_set() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(ArtifactFormat::Json);
        set.insert(ArtifactFormat::Csv);
        set.insert(ArtifactFormat::Png);
        assert_eq!(set.len(), 3);
    }
}
