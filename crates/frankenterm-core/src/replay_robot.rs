//! Robot Mode data payloads for replay operations.
//!
//! Defines the typed request/response structures for `ft robot replay.*`
//! commands. These types are the contract between the CLI robot handler
//! and consumers (agent scripts, CI pipelines, MCP bridges).
//!
//! # Commands
//!
//! | Command                   | Request → Response |
//! |---------------------------|--------------------|
//! | `replay.inspect`          | [`InspectRequest`] → [`InspectData`] |
//! | `replay.diff`             | [`DiffRequest`] → [`DiffData`] |
//! | `replay.regression_suite` | [`RegressionSuiteRequest`] → [`RegressionSuiteData`] |
//! | `replay.artifact.list`    | [`ArtifactListRequest`] → [`ArtifactListData`] |
//! | `replay.artifact.inspect` | [`ArtifactInspectRequest`] → [`ArtifactInspectData`] |
//! | `replay.artifact.add`     | [`ArtifactAddRequest`] → [`ArtifactAddData`] |
//! | `replay.artifact.retire`  | [`ArtifactRetireRequest`] → [`ArtifactRetireData`] |
//! | `replay.artifact.prune`   | [`ArtifactPruneRequest`] → [`ArtifactPruneData`] |
//!
//! # Error Codes
//!
//! All replay robot errors use the `replay.*` namespace:
//! - `replay.file_not_found` — trace or artifact file missing
//! - `replay.parse_error` — JSON deserialization failed
//! - `replay.integrity_error` — SHA-256 mismatch
//! - `replay.duplicate` — artifact already registered
//! - `replay.not_found` — artifact not in manifest
//! - `replay.already_retired` — artifact already retired
//! - `replay.schema_mismatch` — unexpected event schema

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::replay_artifact_registry::{
    ArtifactSensitivityTier, ArtifactStatus,
};

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

pub const REPLAY_ERR_FILE_NOT_FOUND: &str = "replay.file_not_found";
pub const REPLAY_ERR_PARSE_ERROR: &str = "replay.parse_error";
pub const REPLAY_ERR_INTEGRITY_ERROR: &str = "replay.integrity_error";
pub const REPLAY_ERR_DUPLICATE: &str = "replay.duplicate";
pub const REPLAY_ERR_NOT_FOUND: &str = "replay.not_found";
pub const REPLAY_ERR_ALREADY_RETIRED: &str = "replay.already_retired";
pub const REPLAY_ERR_SCHEMA_MISMATCH: &str = "replay.schema_mismatch";

// ---------------------------------------------------------------------------
// Command routing
// ---------------------------------------------------------------------------

/// Top-level replay robot command discriminant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayRobotCommand {
    /// Inspect a trace file.
    Inspect,
    /// Run a decision diff between two traces.
    Diff,
    /// Run a regression suite.
    RegressionSuite,
    /// Artifact sub-commands.
    ArtifactList,
    ArtifactInspect,
    ArtifactAdd,
    ArtifactRetire,
    ArtifactPrune,
}

impl ReplayRobotCommand {
    /// Parse a command string like "replay.inspect" into a variant.
    pub fn from_str_command(s: &str) -> Option<Self> {
        match s {
            "replay.inspect" => Some(Self::Inspect),
            "replay.diff" => Some(Self::Diff),
            "replay.regression_suite" => Some(Self::RegressionSuite),
            "replay.artifact.list" => Some(Self::ArtifactList),
            "replay.artifact.inspect" => Some(Self::ArtifactInspect),
            "replay.artifact.add" => Some(Self::ArtifactAdd),
            "replay.artifact.retire" => Some(Self::ArtifactRetire),
            "replay.artifact.prune" => Some(Self::ArtifactPrune),
            _ => None,
        }
    }

    /// Canonical command string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inspect => "replay.inspect",
            Self::Diff => "replay.diff",
            Self::RegressionSuite => "replay.regression_suite",
            Self::ArtifactList => "replay.artifact.list",
            Self::ArtifactInspect => "replay.artifact.inspect",
            Self::ArtifactAdd => "replay.artifact.add",
            Self::ArtifactRetire => "replay.artifact.retire",
            Self::ArtifactPrune => "replay.artifact.prune",
        }
    }
}

// ---------------------------------------------------------------------------
// Generic request envelope
// ---------------------------------------------------------------------------

/// Generic robot request envelope for replay commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayRequest<T> {
    /// The command being invoked.
    pub command: String,
    /// Command-specific arguments.
    pub args: T,
}

// ---------------------------------------------------------------------------
// replay.inspect
// ---------------------------------------------------------------------------

/// Arguments for `replay.inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectRequest {
    /// Path to the trace file.
    pub trace: String,
}

/// Response data for `replay.inspect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InspectData {
    /// Path inspected.
    pub artifact_path: String,
    /// Number of decision events.
    pub event_count: u64,
    /// Distinct pane IDs.
    pub pane_count: u64,
    /// Distinct rule IDs.
    pub rule_count: u64,
    /// Time span of events (ms).
    pub time_span_ms: u64,
    /// Decision type breakdown.
    pub decision_types: Vec<String>,
    /// SHA-256 integrity check passed.
    pub integrity_ok: bool,
}

impl InspectData {
    /// Build from an [`crate::replay_cli::InspectResult`].
    pub fn from_inspect_result(r: &crate::replay_cli::InspectResult) -> Self {
        Self {
            artifact_path: r.artifact_path.clone(),
            event_count: r.event_count,
            pane_count: r.pane_count,
            rule_count: r.rule_count,
            time_span_ms: r.time_span_ms,
            decision_types: r.decision_types.clone(),
            integrity_ok: r.integrity_ok,
        }
    }
}

// ---------------------------------------------------------------------------
// replay.diff
// ---------------------------------------------------------------------------

/// Arguments for `replay.diff`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRequest {
    /// Baseline trace path.
    pub baseline: String,
    /// Candidate trace path.
    pub candidate: String,
    /// Time tolerance (ms) for shifted detection.
    #[serde(default = "default_tolerance")]
    pub tolerance_ms: u64,
    /// Budget TOML path (optional).
    #[serde(default)]
    pub budget: Option<String>,
}

fn default_tolerance() -> u64 {
    100
}

/// Response data for `replay.diff`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffData {
    /// Whether the diff passed the regression gate.
    pub passed: bool,
    /// Exit code (0=pass, 1=regression, 2=invalid, 3=internal).
    pub exit_code: i32,
    /// Number of divergences found.
    pub divergence_count: u64,
    /// Recommendation text.
    pub recommendation: String,
    /// Gate result summary.
    pub gate_result: String,
    /// Divergence severity breakdown.
    pub severity_counts: BTreeMap<String, u64>,
}

// ---------------------------------------------------------------------------
// replay.regression_suite
// ---------------------------------------------------------------------------

/// Arguments for `replay.regression_suite`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionSuiteRequest {
    /// Directory containing the regression suite.
    pub suite_dir: String,
    /// Budget TOML path (optional).
    #[serde(default)]
    pub budget: Option<String>,
}

/// Response data for `replay.regression_suite`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionSuiteData {
    /// Whether all artifacts passed.
    pub passed: bool,
    /// Total artifacts evaluated.
    pub total_artifacts: u64,
    /// Artifacts that passed.
    pub passed_count: u64,
    /// Artifacts that failed.
    pub failed_count: u64,
    /// Artifacts that errored.
    pub errored_count: u64,
    /// Per-artifact results.
    pub results: Vec<ArtifactResultData>,
}

/// Per-artifact result in a regression suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactResultData {
    /// Artifact path.
    pub artifact_path: String,
    /// Whether this artifact passed.
    pub passed: bool,
    /// Gate result summary.
    pub gate_result_summary: String,
    /// Error message if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RegressionSuiteData {
    /// Build from a [`crate::replay_cli::RegressionSuiteResult`].
    pub fn from_suite_result(r: &crate::replay_cli::RegressionSuiteResult) -> Self {
        Self {
            passed: r.overall_pass,
            total_artifacts: r.total_artifacts,
            passed_count: r.passed,
            failed_count: r.failed,
            errored_count: r.errored,
            results: r
                .results
                .iter()
                .map(|a| ArtifactResultData {
                    artifact_path: a.artifact_path.clone(),
                    passed: a.passed,
                    gate_result_summary: a.gate_result_summary.clone(),
                    error: a.error.clone(),
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// replay.artifact.list
// ---------------------------------------------------------------------------

/// Arguments for `replay.artifact.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArtifactListRequest {
    /// Filter by tier: "T1", "T2", "T3".
    #[serde(default)]
    pub tier: Option<String>,
    /// Filter by status: "active", "retired".
    #[serde(default)]
    pub status: Option<String>,
}

/// Response data for `replay.artifact.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactListData {
    /// Number of artifacts returned.
    pub count: u64,
    /// Artifact entries.
    pub artifacts: Vec<ArtifactSummary>,
}

/// Summary of a single artifact in listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub path: String,
    pub label: String,
    pub tier: String,
    pub status: String,
    pub event_count: u64,
    pub size_bytes: u64,
    pub sha256: String,
}

// ---------------------------------------------------------------------------
// replay.artifact.inspect
// ---------------------------------------------------------------------------

/// Arguments for `replay.artifact.inspect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactInspectRequest {
    pub path: String,
}

/// Response data for `replay.artifact.inspect`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactInspectData {
    pub path: String,
    pub label: String,
    pub tier: String,
    pub status: String,
    pub event_count: u64,
    pub decision_count: u64,
    pub size_bytes: u64,
    pub sha256: String,
    pub integrity_ok: bool,
    pub file_exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retire_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// replay.artifact.add
// ---------------------------------------------------------------------------

/// Arguments for `replay.artifact.add`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactAddRequest {
    pub path: String,
    #[serde(default = "default_label")]
    pub label: String,
    #[serde(default = "default_tier_str")]
    pub tier: String,
}

fn default_label() -> String {
    "unlabeled".to_string()
}

fn default_tier_str() -> String {
    "T1".to_string()
}

/// Response data for `replay.artifact.add`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactAddData {
    pub path: String,
    pub sha256: String,
    pub event_count: u64,
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// replay.artifact.retire
// ---------------------------------------------------------------------------

/// Arguments for `replay.artifact.retire`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRetireRequest {
    pub path: String,
    pub reason: String,
}

/// Response data for `replay.artifact.retire`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRetireData {
    pub path: String,
    pub reason: String,
    pub retired_at_ms: u64,
}

// ---------------------------------------------------------------------------
// replay.artifact.prune
// ---------------------------------------------------------------------------

/// Arguments for `replay.artifact.prune`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPruneRequest {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_max_age_days")]
    pub max_age_days: u64,
}

fn default_max_age_days() -> u64 {
    30
}

/// Response data for `replay.artifact.prune`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactPruneData {
    pub pruned_count: u64,
    pub bytes_freed: u64,
    pub dry_run: bool,
    pub pruned_paths: Vec<String>,
}

impl ArtifactPruneData {
    /// Build from a [`crate::replay_artifact_registry::PruneResult`].
    pub fn from_prune_result(r: &crate::replay_artifact_registry::PruneResult) -> Self {
        Self {
            pruned_count: r.pruned_count,
            bytes_freed: r.bytes_freed,
            dry_run: r.dry_run,
            pruned_paths: r.pruned_paths.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch helper
// ---------------------------------------------------------------------------

/// Classifies a raw JSON command string into a replay robot command.
///
/// Returns `None` if the command doesn't belong to the replay namespace.
pub fn classify_replay_command(command: &str) -> Option<ReplayRobotCommand> {
    ReplayRobotCommand::from_str_command(command)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Command parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_all_commands() {
        let commands = [
            ("replay.inspect", ReplayRobotCommand::Inspect),
            ("replay.diff", ReplayRobotCommand::Diff),
            ("replay.regression_suite", ReplayRobotCommand::RegressionSuite),
            ("replay.artifact.list", ReplayRobotCommand::ArtifactList),
            ("replay.artifact.inspect", ReplayRobotCommand::ArtifactInspect),
            ("replay.artifact.add", ReplayRobotCommand::ArtifactAdd),
            ("replay.artifact.retire", ReplayRobotCommand::ArtifactRetire),
            ("replay.artifact.prune", ReplayRobotCommand::ArtifactPrune),
        ];
        for (s, expected) in &commands {
            assert_eq!(ReplayRobotCommand::from_str_command(s), Some(expected.clone()));
        }
    }

    #[test]
    fn parse_unknown_command() {
        assert_eq!(ReplayRobotCommand::from_str_command("replay.unknown"), None);
        assert_eq!(ReplayRobotCommand::from_str_command("other.inspect"), None);
    }

    #[test]
    fn command_as_str_roundtrip() {
        let commands = [
            ReplayRobotCommand::Inspect,
            ReplayRobotCommand::Diff,
            ReplayRobotCommand::RegressionSuite,
            ReplayRobotCommand::ArtifactList,
            ReplayRobotCommand::ArtifactInspect,
            ReplayRobotCommand::ArtifactAdd,
            ReplayRobotCommand::ArtifactRetire,
            ReplayRobotCommand::ArtifactPrune,
        ];
        for cmd in &commands {
            let s = cmd.as_str();
            let restored = ReplayRobotCommand::from_str_command(s).unwrap();
            assert_eq!(&restored, cmd);
        }
    }

    // ── Command serde ────────────────────────────────────────────────────

    #[test]
    fn command_serde_roundtrip() {
        let cmds = [
            ReplayRobotCommand::Inspect,
            ReplayRobotCommand::Diff,
            ReplayRobotCommand::RegressionSuite,
            ReplayRobotCommand::ArtifactList,
        ];
        for cmd in &cmds {
            let json = serde_json::to_string(cmd).unwrap();
            let restored: ReplayRobotCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(&restored, cmd);
        }
    }

    // ── Request envelope serde ───────────────────────────────────────────

    #[test]
    fn inspect_request_serde() {
        let req = ReplayRequest {
            command: "replay.inspect".to_string(),
            args: InspectRequest {
                trace: "test.ftreplay".to_string(),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: ReplayRequest<InspectRequest> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.args.trace, "test.ftreplay");
    }

    #[test]
    fn diff_request_serde() {
        let req = DiffRequest {
            baseline: "b.ftreplay".into(),
            candidate: "c.ftreplay".into(),
            tolerance_ms: 200,
            budget: Some("budget.toml".into()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: DiffRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tolerance_ms, 200);
        assert_eq!(restored.budget.as_deref(), Some("budget.toml"));
    }

    #[test]
    fn diff_request_defaults() {
        let json = r#"{"baseline":"b","candidate":"c"}"#;
        let req: DiffRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tolerance_ms, 100);
        assert_eq!(req.budget, None);
    }

    #[test]
    fn regression_suite_request_serde() {
        let req = RegressionSuiteRequest {
            suite_dir: "tests/regression/replay/".into(),
            budget: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: RegressionSuiteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.suite_dir, "tests/regression/replay/");
    }

    #[test]
    fn artifact_list_request_defaults() {
        let json = "{}";
        let req: ArtifactListRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.tier, None);
        assert_eq!(req.status, None);
    }

    #[test]
    fn artifact_add_request_defaults() {
        let json = r#"{"path":"test.ftreplay"}"#;
        let req: ArtifactAddRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.label, "unlabeled");
        assert_eq!(req.tier, "T1");
    }

    #[test]
    fn artifact_prune_request_defaults() {
        let json = "{}";
        let req: ArtifactPruneRequest = serde_json::from_str(json).unwrap();
        assert!(!req.dry_run);
        assert_eq!(req.max_age_days, 30);
    }

    // ── Response data serde ──────────────────────────────────────────────

    #[test]
    fn inspect_data_serde() {
        let data = InspectData {
            artifact_path: "test.ftreplay".into(),
            event_count: 42,
            pane_count: 3,
            rule_count: 5,
            time_span_ms: 10000,
            decision_types: vec!["pattern_match".into(), "workflow_step".into()],
            integrity_ok: true,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: InspectData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn diff_data_serde() {
        let mut severity = BTreeMap::new();
        severity.insert("critical".into(), 0);
        severity.insert("high".into(), 1);
        let data = DiffData {
            passed: false,
            exit_code: 1,
            divergence_count: 3,
            recommendation: "Fix the divergences".into(),
            gate_result: "Fail".into(),
            severity_counts: severity,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: DiffData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn regression_suite_data_serde() {
        let data = RegressionSuiteData {
            passed: true,
            total_artifacts: 3,
            passed_count: 3,
            failed_count: 0,
            errored_count: 0,
            results: vec![
                ArtifactResultData {
                    artifact_path: "a.ftreplay".into(),
                    passed: true,
                    gate_result_summary: "Pass".into(),
                    error: None,
                },
            ],
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: RegressionSuiteData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn artifact_list_data_serde() {
        let data = ArtifactListData {
            count: 1,
            artifacts: vec![ArtifactSummary {
                path: "test.ftreplay".into(),
                label: "test".into(),
                tier: "T1".into(),
                status: "active".into(),
                event_count: 10,
                size_bytes: 512,
                sha256: "a".repeat(64),
            }],
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactListData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn artifact_inspect_data_serde() {
        let data = ArtifactInspectData {
            path: "test.ftreplay".into(),
            label: "test".into(),
            tier: "T1".into(),
            status: "active".into(),
            event_count: 10,
            decision_count: 3,
            size_bytes: 512,
            sha256: "a".repeat(64),
            integrity_ok: true,
            file_exists: true,
            retire_reason: None,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactInspectData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn artifact_add_data_serde() {
        let data = ArtifactAddData {
            path: "new.ftreplay".into(),
            sha256: "b".repeat(64),
            event_count: 5,
            size_bytes: 256,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactAddData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn artifact_retire_data_serde() {
        let data = ArtifactRetireData {
            path: "old.ftreplay".into(),
            reason: "replaced".into(),
            retired_at_ms: 5000,
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactRetireData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn artifact_prune_data_serde() {
        let data = ArtifactPruneData {
            pruned_count: 2,
            bytes_freed: 1024,
            dry_run: false,
            pruned_paths: vec!["a.ftreplay".into(), "b.ftreplay".into()],
        };
        let json = serde_json::to_string(&data).unwrap();
        let restored: ArtifactPruneData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, data);
    }

    // ── Error code constants ─────────────────────────────────────────────

    #[test]
    fn error_codes_unique() {
        let codes = [
            REPLAY_ERR_FILE_NOT_FOUND,
            REPLAY_ERR_PARSE_ERROR,
            REPLAY_ERR_INTEGRITY_ERROR,
            REPLAY_ERR_DUPLICATE,
            REPLAY_ERR_NOT_FOUND,
            REPLAY_ERR_ALREADY_RETIRED,
            REPLAY_ERR_SCHEMA_MISMATCH,
        ];
        let mut seen = std::collections::HashSet::new();
        for code in &codes {
            assert!(seen.insert(code), "duplicate error code: {code}");
        }
    }

    #[test]
    fn error_codes_namespace() {
        let codes = [
            REPLAY_ERR_FILE_NOT_FOUND,
            REPLAY_ERR_PARSE_ERROR,
            REPLAY_ERR_INTEGRITY_ERROR,
            REPLAY_ERR_DUPLICATE,
            REPLAY_ERR_NOT_FOUND,
            REPLAY_ERR_ALREADY_RETIRED,
            REPLAY_ERR_SCHEMA_MISMATCH,
        ];
        for code in &codes {
            assert!(code.starts_with("replay."), "code should start with 'replay.': {code}");
        }
    }

    // ── classify helper ──────────────────────────────────────────────────

    #[test]
    fn classify_replay_commands() {
        assert!(classify_replay_command("replay.inspect").is_some());
        assert!(classify_replay_command("replay.diff").is_some());
        assert!(classify_replay_command("not.replay").is_none());
        assert!(classify_replay_command("").is_none());
    }

    // ── InspectData from InspectResult ───────────────────────────────────

    #[test]
    fn inspect_data_from_result() {
        use crate::replay_cli::InspectResult;
        use crate::replay_decision_graph::{DecisionEvent, DecisionType};

        let events = vec![DecisionEvent {
            decision_type: DecisionType::PatternMatch,
            rule_id: "r1".into(),
            definition_hash: "d".into(),
            input_hash: "in".into(),
            output_hash: "out".into(),
            timestamp_ms: 100,
            pane_id: 1,
            triggered_by: None,
            overrides: None,
            wall_clock_ms: 0,
            replay_run_id: String::new(),
        }];
        let result = InspectResult::from_events("test.ftreplay", &events);
        let data = InspectData::from_inspect_result(&result);
        assert_eq!(data.event_count, 1);
        assert_eq!(data.artifact_path, "test.ftreplay");
    }

    // ── RegressionSuiteData from RegressionSuiteResult ───────────────────

    #[test]
    fn suite_data_from_result() {
        use crate::replay_cli::{ArtifactResult, RegressionSuiteResult};

        let results = vec![
            ArtifactResult {
                artifact_path: "a.ftreplay".into(),
                passed: true,
                gate_result_summary: "Pass".into(),
                error: None,
            },
            ArtifactResult {
                artifact_path: "b.ftreplay".into(),
                passed: false,
                gate_result_summary: "Fail".into(),
                error: None,
            },
        ];
        let suite = RegressionSuiteResult::from_results(results);
        let data = RegressionSuiteData::from_suite_result(&suite);
        assert_eq!(data.total_artifacts, 2);
        assert_eq!(data.passed_count, 1);
        assert_eq!(data.failed_count, 1);
        assert!(!data.passed);
    }

    // ── ArtifactPruneData from PruneResult ───────────────────────────────

    #[test]
    fn prune_data_from_result() {
        use crate::replay_artifact_registry::PruneResult;

        let result = PruneResult {
            pruned_count: 2,
            pruned_paths: vec!["a.ftreplay".into(), "b.ftreplay".into()],
            bytes_freed: 512,
            dry_run: true,
        };
        let data = ArtifactPruneData::from_prune_result(&result);
        assert_eq!(data.pruned_count, 2);
        assert_eq!(data.bytes_freed, 512);
        assert!(data.dry_run);
    }

    // ── Retired artifact inspect data ────────────────────────────────────

    #[test]
    fn artifact_inspect_retired_has_reason() {
        let data = ArtifactInspectData {
            path: "retired.ftreplay".into(),
            label: "old".into(),
            tier: "T2".into(),
            status: "retired".into(),
            event_count: 5,
            decision_count: 2,
            size_bytes: 256,
            sha256: "c".repeat(64),
            integrity_ok: false,
            file_exists: false,
            retire_reason: Some("replaced by v2".into()),
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("replaced by v2"));
        let restored: ArtifactInspectData = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.retire_reason.as_deref(), Some("replaced by v2"));
    }

    // ── Full request-response envelope test ──────────────────────────────

    #[test]
    fn full_envelope_inspect() {
        let req = ReplayRequest {
            command: "replay.inspect".to_string(),
            args: InspectRequest {
                trace: "trace.ftreplay".into(),
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("replay.inspect"));
        assert!(json.contains("trace.ftreplay"));
    }

    #[test]
    fn full_envelope_diff() {
        let req = ReplayRequest {
            command: "replay.diff".to_string(),
            args: DiffRequest {
                baseline: "base.ftreplay".into(),
                candidate: "cand.ftreplay".into(),
                tolerance_ms: 50,
                budget: None,
            },
        };
        let json = serde_json::to_string(&req).unwrap();
        let restored: ReplayRequest<DiffRequest> = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.args.tolerance_ms, 50);
    }
}
