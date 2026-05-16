//! Bundled demo scenario manifest contract.
//!
//! The `ft demo` command advertises named scenarios such as `quickstart`,
//! `usage_limit`, and `compaction`. This module defines the versioned manifest
//! contract those bundled demos must satisfy before the CLI starts executing
//! them as first-class assets.

use std::collections::HashSet;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Current demo scenario manifest schema version.
pub const DEMO_SCENARIO_MANIFEST_SCHEMA_VERSION: &str = "ft.demo.scenario-manifest.v1";

/// Maximum bytes any single declared demo artifact may emit.
pub const DEMO_SCENARIO_MAX_ARTIFACT_BYTES: u64 = 1_048_576;

const REQUIRED_DEGRADATION_REASONS: [DemoScenarioDegradationReason; 4] = [
    DemoScenarioDegradationReason::AgentMailUnavailable,
    DemoScenarioDegradationReason::DisabledFeature,
    DemoScenarioDegradationReason::RchProofUnavailable,
    DemoScenarioDegradationReason::UnsupportedPlatform,
];

/// Error returned when a demo scenario manifest fails validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DemoScenarioManifestError {
    /// Manifest could not be parsed as JSON.
    #[error("demo scenario manifest JSON parse failed: {0}")]
    Json(String),
    /// Manifest uses an unsupported schema version.
    #[error("unsupported demo scenario manifest schema version `{0}`")]
    UnsupportedSchemaVersion(String),
    /// Manifest does not declare any scenarios.
    #[error("demo scenario manifest must declare at least one scenario")]
    EmptyManifest,
    /// A required string field is blank.
    #[error("demo scenario `{scenario_id}` has empty field `{field}`")]
    EmptyField {
        /// Scenario id, or `<manifest>` for manifest-level fields.
        scenario_id: String,
        /// Field name.
        field: &'static str,
    },
    /// Scenario id is not stable and machine-safe.
    #[error("demo scenario id `{0}` must be lowercase ascii, digit, `_`, or `-`")]
    InvalidScenarioId(String),
    /// A scenario id appears more than once.
    #[error("duplicate demo scenario id `{0}`")]
    DuplicateScenarioId(String),
    /// A path is absolute, traverses upward, or is otherwise unsafe.
    #[error("demo scenario `{scenario_id}` has unsafe path in `{field}`: {path}")]
    UnsafePath {
        /// Scenario id.
        scenario_id: String,
        /// Field name.
        field: &'static str,
        /// Rejected path.
        path: String,
    },
    /// Scenario path must point to an existing scenario-format extension.
    #[error("demo scenario `{scenario_id}` scenario path must end in .yaml or .yml: {path}")]
    UnsupportedScenarioExtension {
        /// Scenario id.
        scenario_id: String,
        /// Rejected path.
        path: String,
    },
    /// A list field that must not be empty is empty.
    #[error("demo scenario `{scenario_id}` must include at least one `{field}` entry")]
    EmptyList {
        /// Scenario id.
        scenario_id: String,
        /// Field name.
        field: &'static str,
    },
    /// A required degradation reason is missing.
    #[error("demo scenario `{scenario_id}` is missing degradation reason `{reason}`")]
    MissingDegradationReason {
        /// Scenario id.
        scenario_id: String,
        /// Missing reason.
        reason: DemoScenarioDegradationReason,
    },
    /// An artifact byte budget is zero or exceeds the manifest cap.
    #[error(
        "demo scenario `{scenario_id}` artifact `{artifact_id}` has invalid max_bytes {max_bytes}"
    )]
    InvalidArtifactBudget {
        /// Scenario id.
        scenario_id: String,
        /// Artifact id.
        artifact_id: String,
        /// Rejected byte budget.
        max_bytes: u64,
    },
    /// A secret-shaped token appears in manifest text.
    #[error("demo scenario `{scenario_id}` contains secret-shaped text in `{field}`")]
    SecretShapedText {
        /// Scenario id.
        scenario_id: String,
        /// Field name.
        field: &'static str,
    },
}

/// Top-level manifest for bundled demo scenarios.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemoScenarioManifest {
    /// Schema version. Must equal [`DEMO_SCENARIO_MANIFEST_SCHEMA_VERSION`].
    pub schema_version: String,
    /// Human-readable manifest title.
    pub title: String,
    /// Manifest-level statement that demo fixtures are not production-capacity proof.
    pub proof_boundary: String,
    /// Bundled scenarios.
    pub scenarios: Vec<DemoScenarioSpec>,
}

impl DemoScenarioManifest {
    /// Parse and validate a demo scenario manifest from JSON.
    pub fn from_json(json: &str) -> Result<Self, DemoScenarioManifestError> {
        let manifest: Self = serde_json::from_str(json)
            .map_err(|err| DemoScenarioManifestError::Json(err.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Validate this manifest against the v1 contract.
    pub fn validate(&self) -> Result<(), DemoScenarioManifestError> {
        if self.schema_version != DEMO_SCENARIO_MANIFEST_SCHEMA_VERSION {
            return Err(DemoScenarioManifestError::UnsupportedSchemaVersion(
                self.schema_version.clone(),
            ));
        }
        validate_non_empty("<manifest>", "title", &self.title)?;
        validate_non_empty("<manifest>", "proof_boundary", &self.proof_boundary)?;
        reject_secret_shaped_text("<manifest>", "proof_boundary", &self.proof_boundary)?;

        if self.scenarios.is_empty() {
            return Err(DemoScenarioManifestError::EmptyManifest);
        }

        let mut ids = HashSet::new();
        for scenario in &self.scenarios {
            scenario.validate()?;
            if !ids.insert(scenario.id.clone()) {
                return Err(DemoScenarioManifestError::DuplicateScenarioId(
                    scenario.id.clone(),
                ));
            }
        }

        Ok(())
    }
}

/// One bundled demo scenario entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemoScenarioSpec {
    /// Stable machine id, matching the `ft demo <name>` argument.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// What this scenario proves for onboarding or regression.
    pub purpose: String,
    /// Relative path to the scenario YAML asset.
    pub scenario_path: String,
    /// Deterministic seed used by generated fixture data.
    pub deterministic_seed: String,
    /// Required feature flags or capabilities.
    pub required_features: Vec<String>,
    /// Output formats this scenario must support.
    pub supported_outputs: Vec<DemoScenarioOutput>,
    /// Redaction tier required before persisted/exported output can ship.
    pub redaction_tier: DemoScenarioRedactionTier,
    /// Proof category for release-attestation bookkeeping.
    pub proof_category: DemoScenarioProofCategory,
    /// Maximum total output bytes allowed for one scenario run.
    pub max_output_bytes: u64,
    /// Artifacts expected from a validated run.
    pub expected_artifacts: Vec<DemoScenarioArtifact>,
    /// Required degradation behavior.
    pub degradation: Vec<DemoScenarioDegradation>,
}

impl DemoScenarioSpec {
    /// Validate this scenario entry.
    pub fn validate(&self) -> Result<(), DemoScenarioManifestError> {
        validate_scenario_id(&self.id)?;
        validate_non_empty(&self.id, "title", &self.title)?;
        validate_non_empty(&self.id, "purpose", &self.purpose)?;
        validate_non_empty(&self.id, "scenario_path", &self.scenario_path)?;
        validate_non_empty(&self.id, "deterministic_seed", &self.deterministic_seed)?;
        reject_secret_shaped_text(&self.id, "title", &self.title)?;
        reject_secret_shaped_text(&self.id, "purpose", &self.purpose)?;
        reject_secret_shaped_text(&self.id, "deterministic_seed", &self.deterministic_seed)?;
        validate_relative_path(&self.id, "scenario_path", &self.scenario_path)?;
        validate_scenario_extension(&self.id, &self.scenario_path)?;

        if self.required_features.is_empty() {
            return Err(DemoScenarioManifestError::EmptyList {
                scenario_id: self.id.clone(),
                field: "required_features",
            });
        }
        for feature in &self.required_features {
            validate_non_empty(&self.id, "required_features", feature)?;
            reject_secret_shaped_text(&self.id, "required_features", feature)?;
        }

        if self.supported_outputs.is_empty() {
            return Err(DemoScenarioManifestError::EmptyList {
                scenario_id: self.id.clone(),
                field: "supported_outputs",
            });
        }
        if self.expected_artifacts.is_empty() {
            return Err(DemoScenarioManifestError::EmptyList {
                scenario_id: self.id.clone(),
                field: "expected_artifacts",
            });
        }
        if self.degradation.is_empty() {
            return Err(DemoScenarioManifestError::EmptyList {
                scenario_id: self.id.clone(),
                field: "degradation",
            });
        }
        validate_artifact_budget(&self.id, "scenario", self.max_output_bytes)?;

        for artifact in &self.expected_artifacts {
            artifact.validate(&self.id)?;
        }
        for degradation in &self.degradation {
            degradation.validate(&self.id)?;
        }
        for reason in REQUIRED_DEGRADATION_REASONS {
            if !self
                .degradation
                .iter()
                .any(|degradation| degradation.reason == reason)
            {
                return Err(DemoScenarioManifestError::MissingDegradationReason {
                    scenario_id: self.id.clone(),
                    reason,
                });
            }
        }

        Ok(())
    }
}

/// Output formats supported by a demo scenario.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DemoScenarioOutput {
    /// Human-readable CLI output.
    Human,
    /// JSON envelope.
    Json,
    /// TOON-compatible machine envelope.
    Toon,
    /// Structured JSONL proof logs.
    Jsonl,
}

/// Redaction tier required for persisted demo output.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DemoScenarioRedactionTier {
    /// Public, non-sensitive data.
    T0Public,
    /// Standard demo-safe redacted output.
    T1Standard,
    /// Operator-private output requiring explicit retention justification.
    T2Restricted,
}

/// Proof category represented by a scenario.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DemoScenarioProofCategory {
    /// Contract/schema conformance.
    Conformance,
    /// Golden artifact regression.
    Golden,
    /// End-to-end no-mock scenario execution.
    E2e,
}

/// Expected artifact emitted or checked by a scenario.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemoScenarioArtifact {
    /// Stable artifact id.
    pub id: String,
    /// Artifact kind.
    pub kind: DemoScenarioArtifactKind,
    /// Relative artifact path.
    pub path: String,
    /// Maximum byte budget for this artifact.
    pub max_bytes: u64,
    /// Whether the artifact must carry or cite a content hash.
    #[serde(default)]
    pub content_hash_required: bool,
}

impl DemoScenarioArtifact {
    fn validate(&self, scenario_id: &str) -> Result<(), DemoScenarioManifestError> {
        validate_non_empty(scenario_id, "artifact.id", &self.id)?;
        validate_non_empty(scenario_id, "artifact.path", &self.path)?;
        validate_scenario_id(&self.id)?;
        validate_relative_path(scenario_id, "artifact.path", &self.path)?;
        validate_artifact_budget(scenario_id, &self.id, self.max_bytes)?;
        reject_secret_shaped_text(scenario_id, "artifact.id", &self.id)?;
        reject_secret_shaped_text(scenario_id, "artifact.path", &self.path)
    }
}

/// Demo artifact kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DemoScenarioArtifactKind {
    /// Scenario manifest.
    Manifest,
    /// Scenario YAML.
    ScenarioYaml,
    /// Golden JSON output.
    GoldenJson,
    /// Golden TOON output.
    GoldenToon,
    /// Structured JSONL log.
    StructuredLog,
    /// Proof summary.
    ProofSummary,
}

/// One explicitly documented degradation behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DemoScenarioDegradation {
    /// Degradation reason code.
    pub reason: DemoScenarioDegradationReason,
    /// Expected machine status emitted by the runner.
    pub status: DemoScenarioDegradationStatus,
    /// Operator-facing action or explanation.
    pub operator_action: String,
}

impl DemoScenarioDegradation {
    fn validate(&self, scenario_id: &str) -> Result<(), DemoScenarioManifestError> {
        validate_non_empty(
            scenario_id,
            "degradation.operator_action",
            &self.operator_action,
        )?;
        reject_secret_shaped_text(
            scenario_id,
            "degradation.operator_action",
            &self.operator_action,
        )
    }
}

/// Required degradation reason codes.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DemoScenarioDegradationReason {
    /// Agent Mail is unavailable or corrupt.
    AgentMailUnavailable,
    /// Optional feature is disabled.
    DisabledFeature,
    /// RCH cannot provide proof.
    RchProofUnavailable,
    /// Platform cannot run the scenario.
    UnsupportedPlatform,
}

impl std::fmt::Display for DemoScenarioDegradationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AgentMailUnavailable => "agent_mail_unavailable",
            Self::DisabledFeature => "disabled_feature",
            Self::RchProofUnavailable => "rch_proof_unavailable",
            Self::UnsupportedPlatform => "unsupported_platform",
        })
    }
}

/// Machine status for a degraded scenario run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DemoScenarioDegradationStatus {
    /// Scenario can continue with explicit reduced evidence.
    Degraded,
    /// Scenario cannot run on this platform/configuration.
    Unavailable,
    /// Proof was not produced and must not be counted.
    ProofBlocked,
}

fn validate_scenario_id(id: &str) -> Result<(), DemoScenarioManifestError> {
    if id.is_empty()
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
    {
        return Err(DemoScenarioManifestError::InvalidScenarioId(id.to_string()));
    }
    Ok(())
}

fn validate_non_empty(
    scenario_id: &str,
    field: &'static str,
    value: &str,
) -> Result<(), DemoScenarioManifestError> {
    if value.trim().is_empty() {
        return Err(DemoScenarioManifestError::EmptyField {
            scenario_id: scenario_id.to_string(),
            field,
        });
    }
    Ok(())
}

fn validate_relative_path(
    scenario_id: &str,
    field: &'static str,
    path: &str,
) -> Result<(), DemoScenarioManifestError> {
    let parsed = Path::new(path);
    if path.contains('\\')
        || path.contains(':')
        || parsed.is_absolute()
        || parsed.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(DemoScenarioManifestError::UnsafePath {
            scenario_id: scenario_id.to_string(),
            field,
            path: path.to_string(),
        });
    }
    Ok(())
}

fn validate_scenario_extension(
    scenario_id: &str,
    path: &str,
) -> Result<(), DemoScenarioManifestError> {
    let extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str());
    if !matches!(extension, Some("yaml" | "yml")) {
        return Err(DemoScenarioManifestError::UnsupportedScenarioExtension {
            scenario_id: scenario_id.to_string(),
            path: path.to_string(),
        });
    }
    Ok(())
}

fn validate_artifact_budget(
    scenario_id: &str,
    artifact_id: &str,
    max_bytes: u64,
) -> Result<(), DemoScenarioManifestError> {
    if max_bytes == 0 || max_bytes > DEMO_SCENARIO_MAX_ARTIFACT_BYTES {
        return Err(DemoScenarioManifestError::InvalidArtifactBudget {
            scenario_id: scenario_id.to_string(),
            artifact_id: artifact_id.to_string(),
            max_bytes,
        });
    }
    Ok(())
}

fn reject_secret_shaped_text(
    scenario_id: &str,
    field: &'static str,
    value: &str,
) -> Result<(), DemoScenarioManifestError> {
    let lower = value.to_ascii_lowercase();
    let secret_like = lower.contains("sk-")
        || lower.contains("bearer ")
        || lower.contains("begin private key")
        || lower.contains("password=")
        || lower.contains("api_key")
        || lower.contains("secret=");
    if secret_like {
        return Err(DemoScenarioManifestError::SecretShapedText {
            scenario_id: scenario_id.to_string(),
            field,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_MANIFEST: &str = include_str!("../../../fixtures/demo-lab/manifest.v1.json");

    #[test]
    fn bundled_demo_manifest_fixture_validates() {
        let manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        let ids = manifest
            .scenarios
            .iter()
            .map(|scenario| scenario.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["quickstart", "usage_limit", "compaction"]);
    }

    #[test]
    fn demo_manifest_fixture_is_toon_compatible() {
        let manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        let value = serde_json::to_value(manifest).expect("manifest serializes");
        let toon = toon_rust::encode(value.clone(), None);
        let decoded = toon_rust::try_decode(&toon, None).expect("manifest TOON decodes");
        let json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
        let roundtrip: serde_json::Value =
            serde_json::from_str(&json).expect("decoded TOON renders as JSON");
        assert_eq!(roundtrip["schema_version"], value["schema_version"]);
        assert_eq!(roundtrip["scenarios"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn duplicate_ids_fail_closed() {
        let mut manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        manifest.scenarios[1].id = manifest.scenarios[0].id.clone();
        assert!(matches!(
            manifest.validate(),
            Err(DemoScenarioManifestError::DuplicateScenarioId(id)) if id == "quickstart"
        ));
    }

    #[test]
    fn path_traversal_fails_closed() {
        let mut manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        manifest.scenarios[0].scenario_path = "../secret.yaml".to_string();
        assert!(matches!(
            manifest.validate(),
            Err(DemoScenarioManifestError::UnsafePath { scenario_id, field, .. })
                if scenario_id == "quickstart" && field == "scenario_path"
        ));
    }

    #[test]
    fn platform_specific_paths_fail_closed() {
        let mut manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        manifest.scenarios[0].scenario_path = r"C:\secret.yaml".to_string();
        assert!(matches!(
            manifest.validate(),
            Err(DemoScenarioManifestError::UnsafePath { scenario_id, field, .. })
                if scenario_id == "quickstart" && field == "scenario_path"
        ));
    }

    #[test]
    fn missing_required_degradation_reason_fails_closed() {
        let mut manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        manifest.scenarios[0]
            .degradation
            .retain(|entry| entry.reason != DemoScenarioDegradationReason::RchProofUnavailable);
        assert!(matches!(
            manifest.validate(),
            Err(DemoScenarioManifestError::MissingDegradationReason { scenario_id, reason })
                if scenario_id == "quickstart"
                    && reason == DemoScenarioDegradationReason::RchProofUnavailable
        ));
    }

    #[test]
    fn secret_shaped_text_fails_closed() {
        let mut manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        manifest.scenarios[0].purpose = "prove redaction with bearer token".to_string();
        assert!(matches!(
            manifest.validate(),
            Err(DemoScenarioManifestError::SecretShapedText { scenario_id, field })
                if scenario_id == "quickstart" && field == "purpose"
        ));
    }

    #[test]
    fn oversized_artifact_budget_fails_closed() {
        let mut manifest = DemoScenarioManifest::from_json(FIXTURE_MANIFEST)
            .expect("demo manifest fixture should validate");
        manifest.scenarios[0].expected_artifacts[0].max_bytes =
            DEMO_SCENARIO_MAX_ARTIFACT_BYTES + 1;
        assert!(matches!(
            manifest.validate(),
            Err(DemoScenarioManifestError::InvalidArtifactBudget { scenario_id, artifact_id, .. })
                if scenario_id == "quickstart" && artifact_id == "manifest"
        ));
    }
}
