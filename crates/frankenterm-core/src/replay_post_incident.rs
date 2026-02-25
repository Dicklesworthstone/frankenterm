//! Post-incident feedback loop automation.
//!
//! Bead: ft-og6q6.7.6
//!
//! Automates: incident trace → regression artifact → tracking bead → notification.
//! Ensures every resolved incident becomes a regression test in the replay corpus.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ── Constants ────────────────────────────────────────────────────────────────

pub const INCIDENT_CORPUS_VERSION: &str = "1";

// ── Pipeline Step ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStep {
    HarvestArtifact,
    ValidateArtifact,
    RegisterArtifact,
    CreateBead,
    Notify,
}

impl PipelineStep {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HarvestArtifact => "harvest_artifact",
            Self::ValidateArtifact => "validate_artifact",
            Self::RegisterArtifact => "register_artifact",
            Self::CreateBead => "create_bead",
            Self::Notify => "notify",
        }
    }

    #[must_use]
    pub fn from_str_step(s: &str) -> Option<Self> {
        match s {
            "harvest_artifact" => Some(Self::HarvestArtifact),
            "validate_artifact" => Some(Self::ValidateArtifact),
            "register_artifact" => Some(Self::RegisterArtifact),
            "create_bead" => Some(Self::CreateBead),
            "notify" => Some(Self::Notify),
            _ => None,
        }
    }
}

pub const ALL_STEPS: [PipelineStep; 5] = [
    PipelineStep::HarvestArtifact,
    PipelineStep::ValidateArtifact,
    PipelineStep::RegisterArtifact,
    PipelineStep::CreateBead,
    PipelineStep::Notify,
];

// ── Pipeline Input ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostIncidentInput {
    pub incident_id: String,
    pub recording_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
}

/// Validate pipeline input.
#[must_use]
pub fn validate_input(input: &PostIncidentInput) -> Result<(), String> {
    if input.incident_id.is_empty() {
        return Err("incident_id is required".into());
    }
    if input.recording_path.is_empty() {
        return Err("recording_path is required".into());
    }
    if !input.recording_path.ends_with(".ftreplay") {
        return Err(format!(
            "recording_path must end with .ftreplay, got: {}",
            input.recording_path
        ));
    }
    Ok(())
}

// ── Pipeline Step Result ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepResult {
    pub step: PipelineStep,
    pub success: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bead_id: Option<String>,
    pub duration_ms: u64,
}

// ── Pipeline Result ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineResult {
    pub incident_id: String,
    pub success: bool,
    pub steps: Vec<StepResult>,
    pub artifact_path: Option<String>,
    pub bead_id: Option<String>,
    pub total_duration_ms: u64,
    pub error: Option<String>,
}

impl PipelineResult {
    /// Build from completed steps.
    #[must_use]
    pub fn from_steps(incident_id: String, steps: Vec<StepResult>) -> Self {
        let success = steps.iter().all(|s| s.success);
        let total_duration_ms = steps.iter().map(|s| s.duration_ms).sum();
        let artifact_path = steps.iter().rev().find_map(|s| s.artifact_path.clone());
        let bead_id = steps.iter().rev().find_map(|s| s.bead_id.clone());
        let error = if success {
            None
        } else {
            steps.iter().find(|s| !s.success).map(|s| s.message.clone())
        };

        Self {
            incident_id,
            success,
            steps,
            artifact_path,
            bead_id,
            total_duration_ms,
            error,
        }
    }
}

// ── Simulated Pipeline Execution ─────────────────────────────────────────────

/// Execute post-incident pipeline (simulated / dry-run).
///
/// In production, each step would call actual subsystems.
/// Here we validate input and produce the expected output structure.
#[must_use]
pub fn execute_pipeline(input: &PostIncidentInput) -> PipelineResult {
    if let Err(err) = validate_input(input) {
        return PipelineResult {
            incident_id: input.incident_id.clone(),
            success: false,
            steps: vec![],
            artifact_path: None,
            bead_id: None,
            total_duration_ms: 0,
            error: Some(err),
        };
    }

    let artifact_path = format!(
        "evidence/incidents/{}/{}",
        input.incident_id,
        input
            .recording_path
            .rsplit('/')
            .next()
            .unwrap_or(&input.recording_path)
    );
    let bead_id = format!("incident-{}", input.incident_id);

    let steps = vec![
        StepResult {
            step: PipelineStep::HarvestArtifact,
            success: true,
            message: format!("Harvested artifact from {}", input.recording_path),
            artifact_path: Some(artifact_path.clone()),
            bead_id: None,
            duration_ms: 100,
        },
        StepResult {
            step: PipelineStep::ValidateArtifact,
            success: true,
            message: "Artifact validated: schema OK, integrity OK".into(),
            artifact_path: None,
            bead_id: None,
            duration_ms: 50,
        },
        StepResult {
            step: PipelineStep::RegisterArtifact,
            success: true,
            message: format!("Registered artifact at {}", artifact_path),
            artifact_path: Some(artifact_path.clone()),
            bead_id: None,
            duration_ms: 75,
        },
        StepResult {
            step: PipelineStep::CreateBead,
            success: true,
            message: format!("Created tracking bead {}", bead_id),
            artifact_path: None,
            bead_id: Some(bead_id.clone()),
            duration_ms: 50,
        },
        StepResult {
            step: PipelineStep::Notify,
            success: true,
            message: "Notification sent".into(),
            artifact_path: None,
            bead_id: None,
            duration_ms: 25,
        },
    ];

    PipelineResult::from_steps(input.incident_id.clone(), steps)
}

// ── Incident Corpus ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentCorpusEntry {
    pub incident_id: String,
    pub artifact_path: Option<String>,
    pub bead_id: Option<String>,
    pub status: IncidentCoverageStatus,
    pub registered_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentCoverageStatus {
    Covered,
    Pending,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentCorpus {
    pub version: String,
    pub entries: BTreeMap<String, IncidentCorpusEntry>,
}

impl IncidentCorpus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: INCIDENT_CORPUS_VERSION.into(),
            entries: BTreeMap::new(),
        }
    }

    /// Register an incident with its artifact.
    pub fn register(&mut self, incident_id: &str, artifact_path: &str, bead_id: &str) {
        self.entries.insert(
            incident_id.into(),
            IncidentCorpusEntry {
                incident_id: incident_id.into(),
                artifact_path: Some(artifact_path.into()),
                bead_id: Some(bead_id.into()),
                status: IncidentCoverageStatus::Covered,
                registered_at: Some("now".into()),
            },
        );
    }

    /// Register an incident without an artifact (gap).
    pub fn register_gap(&mut self, incident_id: &str) {
        self.entries
            .entry(incident_id.into())
            .or_insert_with(|| IncidentCorpusEntry {
                incident_id: incident_id.into(),
                artifact_path: None,
                bead_id: None,
                status: IncidentCoverageStatus::Missing,
                registered_at: None,
            });
    }

    /// Get coverage status for an incident.
    #[must_use]
    pub fn coverage(&self, incident_id: &str) -> IncidentCoverageStatus {
        self.entries
            .get(incident_id)
            .map_or(IncidentCoverageStatus::Missing, |e| e.status)
    }

    /// Detect gaps: incidents without artifacts.
    #[must_use]
    pub fn gaps(&self) -> Vec<&IncidentCorpusEntry> {
        self.entries
            .values()
            .filter(|e| e.status == IncidentCoverageStatus::Missing)
            .collect()
    }

    /// Count covered incidents.
    #[must_use]
    pub fn covered_count(&self) -> usize {
        self.entries
            .values()
            .filter(|e| e.status == IncidentCoverageStatus::Covered)
            .count()
    }

    /// Coverage percentage.
    #[must_use]
    pub fn coverage_percent(&self) -> f64 {
        if self.entries.is_empty() {
            return 100.0;
        }
        (self.covered_count() as f64 / self.entries.len() as f64) * 100.0
    }

    /// Generate coverage report.
    #[must_use]
    pub fn coverage_report(&self) -> CoverageReport {
        CoverageReport {
            total_incidents: self.entries.len(),
            covered: self.covered_count(),
            pending: self
                .entries
                .values()
                .filter(|e| e.status == IncidentCoverageStatus::Pending)
                .count(),
            missing: self.gaps().len(),
            coverage_percent: self.coverage_percent(),
            gap_incident_ids: self.gaps().iter().map(|e| e.incident_id.clone()).collect(),
        }
    }
}

impl Default for IncidentCorpus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageReport {
    pub total_incidents: usize,
    pub covered: usize,
    pub pending: usize,
    pub missing: usize,
    pub coverage_percent: f64,
    pub gap_incident_ids: Vec<String>,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input() -> PostIncidentInput {
        PostIncidentInput {
            incident_id: "INC-001".into(),
            recording_path: "recordings/inc001.ftreplay".into(),
            severity: Some("high".into()),
            description: Some("Production crash".into()),
            resolved_at: Some("2026-01-01T12:00:00Z".into()),
            webhook_url: None,
        }
    }

    // ── Pipeline Step ────────────────────────────────────────────────────

    #[test]
    fn step_str_roundtrip() {
        for step in &ALL_STEPS {
            let s = step.as_str();
            let parsed = PipelineStep::from_str_step(s);
            assert_eq!(parsed, Some(*step));
        }
    }

    #[test]
    fn step_unknown_returns_none() {
        assert_eq!(PipelineStep::from_str_step("unknown"), None);
    }

    // ── Input Validation ─────────────────────────────────────────────────

    #[test]
    fn valid_input() {
        let input = sample_input();
        assert!(validate_input(&input).is_ok());
    }

    #[test]
    fn empty_incident_id_error() {
        let input = PostIncidentInput {
            incident_id: "".into(),
            recording_path: "test.ftreplay".into(),
            ..sample_input()
        };
        let result = validate_input(&input);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("incident_id"));
    }

    #[test]
    fn empty_recording_path_error() {
        let input = PostIncidentInput {
            recording_path: "".into(),
            ..sample_input()
        };
        let result = validate_input(&input);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("recording_path"));
    }

    #[test]
    fn wrong_extension_error() {
        let input = PostIncidentInput {
            recording_path: "test.json".into(),
            ..sample_input()
        };
        let result = validate_input(&input);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains(".ftreplay"));
    }

    // ── Pipeline Execution ───────────────────────────────────────────────

    #[test]
    fn pipeline_success() {
        let input = sample_input();
        let result = execute_pipeline(&input);
        assert!(result.success);
        assert_eq!(result.steps.len(), 5);
        assert!(result.artifact_path.is_some());
        assert!(result.bead_id.is_some());
        assert!(result.error.is_none());
    }

    #[test]
    fn pipeline_invalid_input_fails() {
        let input = PostIncidentInput {
            incident_id: "".into(),
            recording_path: "test.ftreplay".into(),
            ..sample_input()
        };
        let result = execute_pipeline(&input);
        assert!(!result.success);
        assert!(result.error.is_some());
        assert!(result.steps.is_empty());
    }

    #[test]
    fn pipeline_artifact_path_contains_incident_id() {
        let input = sample_input();
        let result = execute_pipeline(&input);
        assert!(result.artifact_path.unwrap().contains("INC-001"));
    }

    #[test]
    fn pipeline_bead_id_contains_incident_id() {
        let input = sample_input();
        let result = execute_pipeline(&input);
        assert!(result.bead_id.unwrap().contains("INC-001"));
    }

    #[test]
    fn pipeline_total_duration_is_sum() {
        let input = sample_input();
        let result = execute_pipeline(&input);
        let sum: u64 = result.steps.iter().map(|s| s.duration_ms).sum();
        assert_eq!(result.total_duration_ms, sum);
    }

    #[test]
    fn pipeline_idempotent() {
        let input = sample_input();
        let r1 = execute_pipeline(&input);
        let r2 = execute_pipeline(&input);
        assert_eq!(r1.artifact_path, r2.artifact_path);
        assert_eq!(r1.bead_id, r2.bead_id);
    }

    #[test]
    fn pipeline_result_serde_roundtrip() {
        let input = sample_input();
        let result = execute_pipeline(&input);
        let json = serde_json::to_string(&result).unwrap();
        let restored: PipelineResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, result);
    }

    // ── Incident Corpus ──────────────────────────────────────────────────

    #[test]
    fn corpus_empty() {
        let corpus = IncidentCorpus::new();
        assert_eq!(corpus.entries.len(), 0);
        assert!((corpus.coverage_percent() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn corpus_register_and_query() {
        let mut corpus = IncidentCorpus::new();
        corpus.register("INC-001", "/path/artifact.ftreplay", "bead-001");
        assert_eq!(corpus.coverage("INC-001"), IncidentCoverageStatus::Covered);
        assert_eq!(corpus.covered_count(), 1);
    }

    #[test]
    fn corpus_gap_detection() {
        let mut corpus = IncidentCorpus::new();
        corpus.register("INC-001", "/path/a.ftreplay", "bead-001");
        corpus.register_gap("INC-002");
        let gaps = corpus.gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].incident_id, "INC-002");
    }

    #[test]
    fn corpus_coverage_percent() {
        let mut corpus = IncidentCorpus::new();
        corpus.register("INC-001", "/a.ftreplay", "b-001");
        corpus.register("INC-002", "/b.ftreplay", "b-002");
        corpus.register_gap("INC-003");
        let pct = corpus.coverage_percent();
        assert!((pct - 66.66666666666667).abs() < 0.1);
    }

    #[test]
    fn corpus_unknown_incident_is_missing() {
        let corpus = IncidentCorpus::new();
        assert_eq!(
            corpus.coverage("nonexistent"),
            IncidentCoverageStatus::Missing
        );
    }

    #[test]
    fn corpus_register_gap_idempotent() {
        let mut corpus = IncidentCorpus::new();
        corpus.register_gap("INC-001");
        corpus.register_gap("INC-001");
        assert_eq!(corpus.entries.len(), 1);
    }

    #[test]
    fn corpus_register_overwrites_gap() {
        let mut corpus = IncidentCorpus::new();
        corpus.register_gap("INC-001");
        assert_eq!(corpus.coverage("INC-001"), IncidentCoverageStatus::Missing);
        corpus.register("INC-001", "/a.ftreplay", "b-001");
        assert_eq!(corpus.coverage("INC-001"), IncidentCoverageStatus::Covered);
    }

    #[test]
    fn corpus_serde_roundtrip() {
        let mut corpus = IncidentCorpus::new();
        corpus.register("INC-001", "/a.ftreplay", "b-001");
        corpus.register_gap("INC-002");
        let json = serde_json::to_string(&corpus).unwrap();
        let restored: IncidentCorpus = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, corpus);
    }

    // ── Coverage Report ──────────────────────────────────────────────────

    #[test]
    fn coverage_report_counts() {
        let mut corpus = IncidentCorpus::new();
        corpus.register("INC-001", "/a.ftreplay", "b-001");
        corpus.register("INC-002", "/b.ftreplay", "b-002");
        corpus.register_gap("INC-003");
        let report = corpus.coverage_report();
        assert_eq!(report.total_incidents, 3);
        assert_eq!(report.covered, 2);
        assert_eq!(report.missing, 1);
        assert_eq!(report.gap_incident_ids, vec!["INC-003"]);
    }

    #[test]
    fn coverage_report_serde_roundtrip() {
        let mut corpus = IncidentCorpus::new();
        corpus.register("INC-001", "/a.ftreplay", "b-001");
        let report = corpus.coverage_report();
        let json = serde_json::to_string(&report).unwrap();
        let restored: CoverageReport = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.total_incidents, report.total_incidents);
        assert_eq!(restored.covered, report.covered);
    }

    // ── PostIncidentInput serde ──────────────────────────────────────────

    #[test]
    fn input_serde_roundtrip() {
        let input = sample_input();
        let json = serde_json::to_string(&input).unwrap();
        let restored: PostIncidentInput = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, input);
    }

    #[test]
    fn input_minimal_serde() {
        let input = PostIncidentInput {
            incident_id: "INC-002".into(),
            recording_path: "test.ftreplay".into(),
            severity: None,
            description: None,
            resolved_at: None,
            webhook_url: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(!json.contains("severity"));
        let restored: PostIncidentInput = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, input);
    }

    // ── StepResult serde ─────────────────────────────────────────────────

    #[test]
    fn step_result_serde_roundtrip() {
        let step = StepResult {
            step: PipelineStep::HarvestArtifact,
            success: true,
            message: "ok".into(),
            artifact_path: Some("/path".into()),
            bead_id: None,
            duration_ms: 100,
        };
        let json = serde_json::to_string(&step).unwrap();
        let restored: StepResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, step);
    }
}
