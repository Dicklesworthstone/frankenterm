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
}
