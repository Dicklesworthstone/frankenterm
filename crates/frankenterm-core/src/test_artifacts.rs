//! Canonical test artifact schema for resize/reflow validation outputs.
//!
//! This module provides a machine-parseable contract for test artifact bundles
//! so CI, dashboards, and triage tooling can rely on one stable structure.

use serde::{Deserialize, Serialize};

/// Stable schema version identifier for test artifact manifests.
pub const TEST_ARTIFACT_SCHEMA_VERSION: &str = "wa.test_artifacts.v1";

/// Stable schema version identifier for scale-lab workload catalog artifacts.
pub const SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION: &str = "ft.scale_lab.workload_catalog.v1";

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

/// Execution substrate used by a scale-lab workload artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleLabEvidenceMode {
    SimulatedReplay,
    LocalReplay,
    RchReplay,
    LiveMux,
}

/// Workload personas used by scale-lab runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScaleLabWorkloadPersona {
    IdleAgents,
    ActiveAgents,
    NoisyAgents,
    RateLimitedAgents,
    TuiHeavy,
    SearchHeavy,
    WorkflowHeavy,
    DistributedPanes,
}

/// Hardware and runtime substrate metadata for a scale-lab run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleLabHostShape {
    pub host_class: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_gib: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage_gib: Option<u32>,
    pub live_mux_available: bool,
}

/// Cargo feature evidence attached to the command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLabFeatureFlags {
    pub default_features: bool,
    #[serde(default)]
    pub enabled: Vec<String>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

/// Command receipt fields required for reproducible scale-lab artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLabCommandEvidence {
    pub command_line: String,
    pub target_dir: String,
    pub feature_flags: ScaleLabFeatureFlags,
}

/// One workload component in a mixed scale-lab run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleLabWorkloadMixEntry {
    pub persona: ScaleLabWorkloadPersona,
    pub pane_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes_per_sec: Option<f64>,
    #[serde(default)]
    pub operations: Vec<String>,
}

/// Timing evidence for a scale-lab artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleLabTimingEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_api_latency_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_api_latency_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_api_latency_ms: Option<f64>,
}

/// Memory evidence for a scale-lab artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLabMemoryEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_tier_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_tier_bytes: Option<u64>,
}

/// Disk evidence for a scale-lab artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLabDiskEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_bytes_after_run: Option<u64>,
}

/// Event/drop/gap counters required for scale-lab proof artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLabEventEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_events: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_gaps: Option<u64>,
}

/// Self-contained workload catalog and proof artifact skeleton for scale-lab runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScaleLabWorkloadCatalog {
    pub schema_version: String,
    pub catalog_id: String,
    pub generated_at_ms: u64,
    #[serde(default)]
    pub field_notes: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_mode: Option<ScaleLabEvidenceMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pane_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<ScaleLabHostShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<ScaleLabCommandEvidence>,
    #[serde(default)]
    pub workload_mix: Vec<ScaleLabWorkloadMixEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<ScaleLabTimingEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<ScaleLabMemoryEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<ScaleLabDiskEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<ScaleLabEventEvidence>,
    #[serde(default)]
    pub limitations: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactEntry>,
}

impl ScaleLabWorkloadCatalog {
    /// Validate that a scale-lab catalog carries enough evidence for later claims.
    pub fn validate(&self) -> Result<(), ScaleLabWorkloadSchemaError> {
        if self.schema_version != SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION {
            return Err(ScaleLabWorkloadSchemaError::InvalidSchemaVersion {
                found: self.schema_version.clone(),
            });
        }
        require_non_empty("catalog_id", &self.catalog_id)?;
        if self.generated_at_ms == 0 {
            return Err(ScaleLabWorkloadSchemaError::NonPositiveField {
                field: "generated_at_ms",
                value: 0,
            });
        }

        let mode = require_some("evidence_mode", self.evidence_mode)?;
        let target_panes = require_some("target_pane_count", self.target_pane_count)?;
        if target_panes == 0 {
            return Err(ScaleLabWorkloadSchemaError::NonPositiveField {
                field: "target_pane_count",
                value: 0,
            });
        }

        let host = require_some_ref("host", &self.host)?;
        validate_host_shape(host, mode)?;
        validate_command(require_some_ref("command", &self.command)?)?;
        validate_workload_mix(&self.workload_mix, target_panes)?;
        validate_timings(require_some_ref("timings", &self.timings)?)?;
        validate_memory(require_some_ref("memory", &self.memory)?)?;
        validate_disk(require_some_ref("disk", &self.disk)?)?;
        validate_events(require_some_ref("events", &self.events)?)?;
        validate_non_empty_strings("limitations", &self.limitations)?;
        validate_scale_lab_artifacts(&self.artifacts)?;

        Ok(())
    }
}

/// Validation errors for [`ScaleLabWorkloadCatalog`].
#[derive(Debug, Clone, PartialEq)]
pub enum ScaleLabWorkloadSchemaError {
    InvalidSchemaVersion { found: String },
    MissingField { field: &'static str },
    EmptyField { field: &'static str },
    NonPositiveField { field: &'static str, value: u64 },
    InvalidNumber { field: &'static str, value: f64 },
    InvalidPercentileOrder { p50: f64, p95: f64, p99: f64 },
    PaneCountMismatch { expected: u32, actual: u32 },
    MissingArtifactPath { index: usize },
    InvalidSha256 { index: usize, value: String },
    MissingWorkloadOperations { persona: ScaleLabWorkloadPersona },
    LiveMuxUnavailable,
}

impl std::fmt::Display for ScaleLabWorkloadSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchemaVersion { found } => {
                write!(f, "invalid scale-lab schema version: {found}")
            }
            Self::MissingField { field } => write!(f, "scale-lab field {field} is required"),
            Self::EmptyField { field } => write!(f, "scale-lab field {field} must not be empty"),
            Self::NonPositiveField { field, value } => {
                write!(f, "scale-lab field {field} must be positive (got {value})")
            }
            Self::InvalidNumber { field, value } => {
                write!(
                    f,
                    "scale-lab numeric field {field} must be finite and non-negative (got {value})"
                )
            }
            Self::InvalidPercentileOrder { p50, p95, p99 } => write!(
                f,
                "invalid scale-lab latency percentile order: expected p50 <= p95 <= p99, got {p50}, {p95}, {p99}"
            ),
            Self::PaneCountMismatch { expected, actual } => write!(
                f,
                "scale-lab workload_mix pane count mismatch: expected {expected}, got {actual}"
            ),
            Self::MissingArtifactPath { index } => {
                write!(f, "scale-lab artifact at index {index} has empty path")
            }
            Self::InvalidSha256 { index, value } => write!(
                f,
                "scale-lab artifact at index {index} has invalid sha256 '{value}'"
            ),
            Self::MissingWorkloadOperations { persona } => {
                write!(f, "scale-lab workload persona {persona:?} needs operations")
            }
            Self::LiveMuxUnavailable => write!(
                f,
                "scale-lab live_mux evidence requires host.live_mux_available=true"
            ),
        }
    }
}

impl std::error::Error for ScaleLabWorkloadSchemaError {}

fn require_some<T>(
    field: &'static str,
    value: Option<T>,
) -> Result<T, ScaleLabWorkloadSchemaError> {
    value.ok_or(ScaleLabWorkloadSchemaError::MissingField { field })
}

fn require_some_ref<'a, T>(
    field: &'static str,
    value: &'a Option<T>,
) -> Result<&'a T, ScaleLabWorkloadSchemaError> {
    value
        .as_ref()
        .ok_or(ScaleLabWorkloadSchemaError::MissingField { field })
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), ScaleLabWorkloadSchemaError> {
    if value.trim().is_empty() {
        return Err(ScaleLabWorkloadSchemaError::EmptyField { field });
    }
    Ok(())
}

fn validate_non_empty_strings(
    field: &'static str,
    values: &[String],
) -> Result<(), ScaleLabWorkloadSchemaError> {
    if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
        return Err(ScaleLabWorkloadSchemaError::EmptyField { field });
    }
    Ok(())
}

fn validate_positive_option(
    field: &'static str,
    value: Option<u32>,
) -> Result<(), ScaleLabWorkloadSchemaError> {
    match value {
        Some(positive) if positive > 0 => Ok(()),
        Some(value) => Err(ScaleLabWorkloadSchemaError::NonPositiveField {
            field,
            value: u64::from(value),
        }),
        None => Err(ScaleLabWorkloadSchemaError::MissingField { field }),
    }
}

fn validate_non_negative_number(
    field: &'static str,
    value: Option<f64>,
) -> Result<f64, ScaleLabWorkloadSchemaError> {
    let value = require_some(field, value)?;
    if !value.is_finite() || value.is_sign_negative() {
        return Err(ScaleLabWorkloadSchemaError::InvalidNumber { field, value });
    }
    Ok(value)
}

fn validate_non_negative_u64(
    field: &'static str,
    value: Option<u64>,
) -> Result<(), ScaleLabWorkloadSchemaError> {
    require_some(field, value)?;
    Ok(())
}

fn validate_host_shape(
    host: &ScaleLabHostShape,
    mode: ScaleLabEvidenceMode,
) -> Result<(), ScaleLabWorkloadSchemaError> {
    require_non_empty("host.host_class", &host.host_class)?;
    require_non_empty("host.os", &host.os)?;
    validate_positive_option("host.cpu_cores", host.cpu_cores)?;
    validate_positive_option("host.memory_gib", host.memory_gib)?;
    validate_positive_option("host.storage_gib", host.storage_gib)?;
    if mode == ScaleLabEvidenceMode::LiveMux && !host.live_mux_available {
        return Err(ScaleLabWorkloadSchemaError::LiveMuxUnavailable);
    }
    Ok(())
}

fn validate_command(command: &ScaleLabCommandEvidence) -> Result<(), ScaleLabWorkloadSchemaError> {
    require_non_empty("command.command_line", &command.command_line)?;
    require_non_empty("command.target_dir", &command.target_dir)?;
    if !command.feature_flags.default_features
        && command.feature_flags.enabled.is_empty()
        && command.feature_flags.disabled.is_empty()
    {
        return Err(ScaleLabWorkloadSchemaError::EmptyField {
            field: "command.feature_flags",
        });
    }
    if command
        .feature_flags
        .enabled
        .iter()
        .chain(command.feature_flags.disabled.iter())
        .any(|flag| flag.trim().is_empty())
    {
        return Err(ScaleLabWorkloadSchemaError::EmptyField {
            field: "command.feature_flags",
        });
    }
    Ok(())
}

fn validate_workload_mix(
    workload_mix: &[ScaleLabWorkloadMixEntry],
    target_pane_count: u32,
) -> Result<(), ScaleLabWorkloadSchemaError> {
    if workload_mix.is_empty() {
        return Err(ScaleLabWorkloadSchemaError::MissingField {
            field: "workload_mix",
        });
    }

    let mut actual_panes = 0_u32;
    for entry in workload_mix {
        if entry.pane_count == 0 {
            return Err(ScaleLabWorkloadSchemaError::NonPositiveField {
                field: "workload_mix.pane_count",
                value: 0,
            });
        }
        validate_non_negative_number(
            "workload_mix.output_bytes_per_sec",
            entry.output_bytes_per_sec,
        )?;
        if entry.operations.is_empty() {
            return Err(ScaleLabWorkloadSchemaError::MissingWorkloadOperations {
                persona: entry.persona,
            });
        }
        validate_non_empty_strings("workload_mix.operations", &entry.operations)?;
        actual_panes = actual_panes.saturating_add(entry.pane_count);
    }

    if actual_panes != target_pane_count {
        return Err(ScaleLabWorkloadSchemaError::PaneCountMismatch {
            expected: target_pane_count,
            actual: actual_panes,
        });
    }

    Ok(())
}

fn validate_timings(timings: &ScaleLabTimingEvidence) -> Result<(), ScaleLabWorkloadSchemaError> {
    validate_non_negative_number("timings.elapsed_ms", timings.elapsed_ms)?;
    let p50 =
        validate_non_negative_number("timings.p50_api_latency_ms", timings.p50_api_latency_ms)?;
    let p95 =
        validate_non_negative_number("timings.p95_api_latency_ms", timings.p95_api_latency_ms)?;
    let p99 =
        validate_non_negative_number("timings.p99_api_latency_ms", timings.p99_api_latency_ms)?;

    if !(p50 <= p95 && p95 <= p99) {
        return Err(ScaleLabWorkloadSchemaError::InvalidPercentileOrder { p50, p95, p99 });
    }

    Ok(())
}

fn validate_memory(memory: &ScaleLabMemoryEvidence) -> Result<(), ScaleLabWorkloadSchemaError> {
    validate_non_negative_u64("memory.peak_rss_bytes", memory.peak_rss_bytes)?;
    validate_non_negative_u64("memory.memory_limit_bytes", memory.memory_limit_bytes)?;
    validate_non_negative_u64("memory.warm_tier_bytes", memory.warm_tier_bytes)?;
    validate_non_negative_u64("memory.cold_tier_bytes", memory.cold_tier_bytes)?;
    Ok(())
}

fn validate_disk(disk: &ScaleLabDiskEvidence) -> Result<(), ScaleLabWorkloadSchemaError> {
    validate_non_negative_u64("disk.bytes_written", disk.bytes_written)?;
    validate_non_negative_u64("disk.free_bytes_after_run", disk.free_bytes_after_run)?;
    Ok(())
}

fn validate_events(events: &ScaleLabEventEvidence) -> Result<(), ScaleLabWorkloadSchemaError> {
    validate_non_negative_u64("events.detection_events", events.detection_events)?;
    validate_non_negative_u64("events.workflow_events", events.workflow_events)?;
    validate_non_negative_u64("events.dropped_events", events.dropped_events)?;
    validate_non_negative_u64("events.capture_gaps", events.capture_gaps)?;
    Ok(())
}

fn validate_scale_lab_artifacts(
    artifacts: &[ArtifactEntry],
) -> Result<(), ScaleLabWorkloadSchemaError> {
    if artifacts.is_empty() {
        return Err(ScaleLabWorkloadSchemaError::MissingField { field: "artifacts" });
    }
    for (index, artifact) in artifacts.iter().enumerate() {
        if artifact.path.trim().is_empty() {
            return Err(ScaleLabWorkloadSchemaError::MissingArtifactPath { index });
        }
        if let Some(hash) = &artifact.sha256 {
            let valid = hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit());
            if !valid {
                return Err(ScaleLabWorkloadSchemaError::InvalidSha256 {
                    index,
                    value: hash.clone(),
                });
            }
        }
    }
    Ok(())
}

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

    fn valid_scale_lab_catalog() -> ScaleLabWorkloadCatalog {
        let mut field_notes = std::collections::BTreeMap::new();
        field_notes.insert(
            "target_pane_count".to_string(),
            "Total pane-equivalent workload covered by workload_mix.".to_string(),
        );
        field_notes.insert(
            "evidence_mode".to_string(),
            "Declares whether evidence is simulated, replayed, rch-offloaded, or live mux."
                .to_string(),
        );

        ScaleLabWorkloadCatalog {
            schema_version: SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION.to_string(),
            catalog_id: "ft-s6h49.scale-lab-smoke".to_string(),
            generated_at_ms: 1_778_087_760_000,
            field_notes,
            evidence_mode: Some(ScaleLabEvidenceMode::RchReplay),
            target_pane_count: Some(10),
            host: Some(ScaleLabHostShape {
                host_class: "rch-worker-smoke".to_string(),
                os: "linux".to_string(),
                cpu_cores: Some(8),
                memory_gib: Some(32),
                storage_gib: Some(256),
                live_mux_available: false,
            }),
            command: Some(ScaleLabCommandEvidence {
                command_line: "rch exec -- env CARGO_TARGET_DIR=/tmp/ft-s6h49-silverharbor-target cargo test -p frankenterm-core scale_lab_workload_catalog --lib --no-default-features".to_string(),
                target_dir: "/tmp/ft-s6h49-silverharbor-target".to_string(),
                feature_flags: ScaleLabFeatureFlags {
                    default_features: false,
                    enabled: vec!["no-default-features".to_string()],
                    disabled: Vec::new(),
                },
            }),
            workload_mix: vec![
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::IdleAgents,
                    pane_count: 1,
                    output_bytes_per_sec: Some(0.0),
                    operations: vec!["state_snapshot".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::ActiveAgents,
                    pane_count: 2,
                    output_bytes_per_sec: Some(256.0),
                    operations: vec!["capture_delta".to_string(), "detect_prompt".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::NoisyAgents,
                    pane_count: 1,
                    output_bytes_per_sec: Some(4096.0),
                    operations: vec!["scan_pipeline".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::RateLimitedAgents,
                    pane_count: 1,
                    output_bytes_per_sec: Some(128.0),
                    operations: vec!["pattern_detection".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::TuiHeavy,
                    pane_count: 1,
                    output_bytes_per_sec: Some(2048.0),
                    operations: vec!["ansi_density_scan".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::SearchHeavy,
                    pane_count: 2,
                    output_bytes_per_sec: Some(512.0),
                    operations: vec!["fts_query".to_string(), "hybrid_query".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::WorkflowHeavy,
                    pane_count: 1,
                    output_bytes_per_sec: Some(384.0),
                    operations: vec!["workflow_trigger".to_string()],
                },
                ScaleLabWorkloadMixEntry {
                    persona: ScaleLabWorkloadPersona::DistributedPanes,
                    pane_count: 1,
                    output_bytes_per_sec: Some(256.0),
                    operations: vec!["stale_session_prune".to_string()],
                },
            ],
            timings: Some(ScaleLabTimingEvidence {
                elapsed_ms: Some(12_000.0),
                p50_api_latency_ms: Some(3.0),
                p95_api_latency_ms: Some(8.0),
                p99_api_latency_ms: Some(13.0),
            }),
            memory: Some(ScaleLabMemoryEvidence {
                peak_rss_bytes: Some(320 * 1024 * 1024),
                memory_limit_bytes: Some(32 * 1024 * 1024 * 1024),
                warm_tier_bytes: Some(12 * 1024 * 1024),
                cold_tier_bytes: Some(0),
            }),
            disk: Some(ScaleLabDiskEvidence {
                bytes_written: Some(4 * 1024 * 1024),
                free_bytes_after_run: Some(180 * 1024 * 1024 * 1024),
            }),
            events: Some(ScaleLabEventEvidence {
                detection_events: Some(7),
                workflow_events: Some(2),
                dropped_events: Some(0),
                capture_gaps: Some(0),
            }),
            limitations: vec![
                "rch replay smoke artifact; not a live mux or 64-core/256GB proof".to_string(),
                "pane count is 10 pane-equivalents and cannot graduate larger support claims"
                    .to_string(),
            ],
            artifacts: vec![
                ArtifactEntry {
                    kind: ArtifactKind::StructuredLog,
                    format: ArtifactFormat::JsonLines,
                    path: "artifacts/scale-lab-smoke/events.jsonl".to_string(),
                    bytes: Some(1024),
                    sha256: Some("0".repeat(64)),
                    redacted: true,
                },
                ArtifactEntry {
                    kind: ArtifactKind::TraceBundle,
                    format: ArtifactFormat::Json,
                    path: "artifacts/scale-lab-smoke/trace.json".to_string(),
                    bytes: Some(2048),
                    sha256: Some("1".repeat(64)),
                    redacted: true,
                },
            ],
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
        manifest.correlation.test_case_id = String::new();
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
            TestArtifactSchemaError::NegativeTiming {
                field: "reflow_ms",
                ..
            }
        ));
    }

    #[test]
    fn negative_queue_wait_rejected() {
        let mut manifest = valid_manifest(ArtifactRunOutcome::Passed);
        manifest.timing.queue_wait_ms = Some(-0.001);
        let err = manifest.validate().unwrap_err();
        assert!(matches!(
            err,
            TestArtifactSchemaError::NegativeTiming {
                field: "queue_wait_ms",
                ..
            }
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
        let err: Box<dyn std::error::Error> = Box::new(TestArtifactSchemaError::MissingRunId);
        assert!(err.to_string().contains("run_id"));
    }

    // =====================================================================
    // Schema version constant
    // =====================================================================

    #[test]
    fn schema_version_constant_is_stable() {
        assert_eq!(TEST_ARTIFACT_SCHEMA_VERSION, "wa.test_artifacts.v1");
    }

    #[test]
    fn scale_lab_schema_version_constant_is_stable() {
        assert_eq!(
            SCALE_LAB_WORKLOAD_CATALOG_SCHEMA_VERSION,
            "ft.scale_lab.workload_catalog.v1"
        );
    }

    #[test]
    fn scale_lab_workload_catalog_validates_required_evidence() {
        let catalog = valid_scale_lab_catalog();
        assert!(catalog.validate().is_ok());
    }

    #[test]
    fn scale_lab_workload_catalog_requires_host_shape() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.host = None;

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::MissingField { field: "host" }
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_rejects_pane_count_mismatch() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.target_pane_count = Some(11);

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::PaneCountMismatch {
                expected: 11,
                actual: 10
            }
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_requires_drop_and_gap_counter_group() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.events = None;

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::MissingField { field: "events" }
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_requires_dropped_event_counter() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.events.as_mut().unwrap().dropped_events = None;

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::MissingField {
                field: "events.dropped_events"
            }
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_rejects_invalid_timing_number() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.timings.as_mut().unwrap().p95_api_latency_ms = Some(f64::NAN);

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::InvalidNumber {
                field: "timings.p95_api_latency_ms",
                ..
            }
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_rejects_live_mux_without_live_mux_host() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.evidence_mode = Some(ScaleLabEvidenceMode::LiveMux);

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::LiveMuxUnavailable
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_rejects_empty_limitations() {
        let mut catalog = valid_scale_lab_catalog();
        catalog.limitations.clear();

        let err = catalog.validate().unwrap_err();
        assert!(matches!(
            err,
            ScaleLabWorkloadSchemaError::EmptyField {
                field: "limitations"
            }
        ));
    }

    #[test]
    fn scale_lab_workload_catalog_fixture_validates() {
        let fixture = include_str!("../../../fixtures/scale-lab/workload-catalog-smoke.v1.json");
        let catalog: ScaleLabWorkloadCatalog = serde_json::from_str(fixture).unwrap();
        assert!(catalog.validate().is_ok());
        assert_eq!(catalog.target_pane_count, Some(10));
        assert_eq!(catalog.workload_mix.len(), 8);
    }

    #[test]
    fn scale_lab_workload_catalog_serde_roundtrip() {
        let catalog = valid_scale_lab_catalog();
        let json = serde_json::to_string(&catalog).unwrap();
        let deserialized: ScaleLabWorkloadCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(catalog, deserialized);
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
