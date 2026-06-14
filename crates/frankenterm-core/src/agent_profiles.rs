//! `agent_profiles` table substrate — typed shape + schema +
//! validation for the robot-mode profile family handler.
//!
//! **Bead:** [BR-RC-ROBOT-CONTRACT.1.cont.handler] / `ft-df3cz`.
//! **Parent:** ft-hac7w.2 (profile family contract — closed).
//! **State-machine model:** [`crate::robot_profile_state_machine`].
//! **Contract doc:** `docs/robot-contracts/profile.md`.
//!
//! # What this module ships
//!
//! - [`AgentProfile`] — typed shape matching the bead's stated
//!   schema: `id, name, role, tags, shell, command, env,
//!   metadata, created_at_ms, updated_at_ms`.
//! - [`AGENT_PROFILES_SCHEMA`] — the `CREATE TABLE` SQL the
//!   migration runner consumes.
//! - [`AgentProfile::validate`] — name + role + length
//!   invariants per the contract doc:
//!   - `name` matches `/^[a-zA-Z0-9_-]+$/`, 1..=64 chars.
//!   - `role` 0..=64 chars (empty allowed).
//!   - tag values 1..=64 chars each.
//!
//! # Wired consumers
//!
//! - The storage migration registers `AGENT_PROFILES_SCHEMA` and the role index
//!   through the schema migration runner.
//! - `RobotCommands::Profile` dispatches through
//!   [`crate::robot_profile_handler`] for list/show/validate/dry-run apply.
//! - Non-dry-run apply uses the mux-backed mutation executor and content-hash
//!   replay receipts in the profile apply handler.
//!
//! The substrate remains in this module; the robot-facing behavior lives in the
//! profile handler and CLI dispatch layer.

use crate::patterns::AgentType;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// `CREATE TABLE` SQL for the `agent_profiles` table. Consumed by the storage
/// migration runner.
///
/// Schema rationale:
/// - `name` is the primary key (TEXT, unique). Profile names
///   are operator-facing identifiers and must be globally
///   unique within an ft installation.
/// - `tags` and `env` are stored as JSON-encoded TEXT to match
///   storage.rs's existing convention for variadic-shape data
///   (see e.g. workflow metadata in storage.rs).
/// - `created_at_ms` / `updated_at_ms` are unix epoch ms `INTEGER`
///   matching storage.rs's hot-path timestamp convention.
pub const AGENT_PROFILES_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS agent_profiles (
    name           TEXT PRIMARY KEY NOT NULL,
    role           TEXT NOT NULL DEFAULT '',
    tags           TEXT NOT NULL DEFAULT '[]',
    shell          TEXT NOT NULL DEFAULT '',
    command        TEXT,
    env            TEXT NOT NULL DEFAULT '{}',
    metadata       TEXT NOT NULL DEFAULT '{}',
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
)";

/// Index on `role` for `list --role <role>` filtering.
pub const AGENT_PROFILES_ROLE_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS agent_profiles_role_idx ON agent_profiles(role)";

/// Maximum length for the `name` field (per contract doc).
pub const NAME_MAX_LEN: usize = 64;

/// Maximum length for the `role` field.
pub const ROLE_MAX_LEN: usize = 64;

/// Maximum length for an individual tag.
pub const TAG_MAX_LEN: usize = 64;

/// Maximum number of tags per profile. DoS guard against
/// huge `tags` arrays imported from operator config.
pub const TAGS_MAX_COUNT: usize = 64;

/// Maximum number of env-var entries.
pub const ENV_MAX_COUNT: usize = 256;

/// Maximum length for an env-var key.
pub const ENV_KEY_MAX_LEN: usize = 256;

/// Maximum length for an env-var value (4 KiB — should fit
/// any reasonable token / URL / path; large values usually
/// indicate misconfiguration or a malicious payload).
pub const ENV_VALUE_MAX_LEN: usize = 4096;

/// Maximum number of metadata entries.
pub const METADATA_MAX_COUNT: usize = 64;

/// Maximum length for a metadata key.
pub const METADATA_KEY_MAX_LEN: usize = 64;

/// Maximum length for a metadata value.
pub const METADATA_VALUE_MAX_LEN: usize = 1024;

/// Maximum length for the `shell` field. Path-style values
/// (`/usr/bin/zsh`, `/run/current-system/sw/bin/fish`) fit
/// well within 256.
pub const SHELL_MAX_LEN: usize = 256;

/// Maximum length for the `command` field. The command is
/// executed when the profile spawns; an extremely long
/// command is almost certainly malicious or a bug. 4 KiB is
/// generous for legitimate shell one-liners.
pub const COMMAND_MAX_LEN: usize = 4096;

/// Typed agent profile row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub shell: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl AgentProfile {
    /// Validate the profile's invariants per the contract doc.
    /// Returns `Ok(())` if valid; otherwise the first violated
    /// invariant.
    pub fn validate(&self) -> Result<(), ProfileValidationError> {
        if self.name.is_empty() {
            return Err(ProfileValidationError::NameEmpty);
        }
        if self.name.len() > NAME_MAX_LEN {
            return Err(ProfileValidationError::NameTooLong {
                len: self.name.len(),
                max: NAME_MAX_LEN,
            });
        }
        for (i, c) in self.name.chars().enumerate() {
            if !is_name_char(c) {
                return Err(ProfileValidationError::NameBadChar {
                    position: i,
                    observed: c,
                });
            }
        }
        if self.role.len() > ROLE_MAX_LEN {
            return Err(ProfileValidationError::RoleTooLong {
                len: self.role.len(),
                max: ROLE_MAX_LEN,
            });
        }
        if self.tags.len() > TAGS_MAX_COUNT {
            return Err(ProfileValidationError::TooManyTags {
                len: self.tags.len(),
                max: TAGS_MAX_COUNT,
            });
        }
        for (i, tag) in self.tags.iter().enumerate() {
            if tag.is_empty() {
                return Err(ProfileValidationError::TagEmpty { position: i });
            }
            if tag.len() > TAG_MAX_LEN {
                return Err(ProfileValidationError::TagTooLong {
                    position: i,
                    len: tag.len(),
                    max: TAG_MAX_LEN,
                });
            }
        }
        if self.shell.len() > SHELL_MAX_LEN {
            return Err(ProfileValidationError::ShellTooLong {
                len: self.shell.len(),
                max: SHELL_MAX_LEN,
            });
        }
        if let Some(ref cmd) = self.command {
            if cmd.len() > COMMAND_MAX_LEN {
                return Err(ProfileValidationError::CommandTooLong {
                    len: cmd.len(),
                    max: COMMAND_MAX_LEN,
                });
            }
        }
        if self.env.len() > ENV_MAX_COUNT {
            return Err(ProfileValidationError::TooManyEnvEntries {
                len: self.env.len(),
                max: ENV_MAX_COUNT,
            });
        }
        for (k, v) in &self.env {
            if k.len() > ENV_KEY_MAX_LEN {
                return Err(ProfileValidationError::EnvKeyTooLong {
                    key_preview: k.chars().take(32).collect(),
                    len: k.len(),
                    max: ENV_KEY_MAX_LEN,
                });
            }
            if v.len() > ENV_VALUE_MAX_LEN {
                return Err(ProfileValidationError::EnvValueTooLong {
                    key: k.clone(),
                    len: v.len(),
                    max: ENV_VALUE_MAX_LEN,
                });
            }
        }
        if self.metadata.len() > METADATA_MAX_COUNT {
            return Err(ProfileValidationError::TooManyMetadataEntries {
                len: self.metadata.len(),
                max: METADATA_MAX_COUNT,
            });
        }
        for (k, v) in &self.metadata {
            if k.len() > METADATA_KEY_MAX_LEN {
                return Err(ProfileValidationError::MetadataKeyTooLong {
                    key_preview: k.chars().take(32).collect(),
                    len: k.len(),
                    max: METADATA_KEY_MAX_LEN,
                });
            }
            if v.len() > METADATA_VALUE_MAX_LEN {
                return Err(ProfileValidationError::MetadataValueTooLong {
                    key: k.clone(),
                    len: v.len(),
                    max: METADATA_VALUE_MAX_LEN,
                });
            }
        }
        if self.created_at_ms < 0 {
            return Err(ProfileValidationError::TimestampNegative {
                field: "created_at_ms",
                value: self.created_at_ms,
            });
        }
        if self.updated_at_ms < 0 {
            return Err(ProfileValidationError::TimestampNegative {
                field: "updated_at_ms",
                value: self.updated_at_ms,
            });
        }
        if self.updated_at_ms < self.created_at_ms {
            return Err(ProfileValidationError::UpdatedBeforeCreated {
                created: self.created_at_ms,
                updated: self.updated_at_ms,
            });
        }
        Ok(())
    }
}

/// Schema version for [`AgentCompatibilityReceipt`].
pub const AGENT_COMPATIBILITY_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Stable contract name for Robot/MCP/CLI surfaces that expose the matrix.
pub const AGENT_COMPATIBILITY_MATRIX_CONTRACT_ID: &str =
    "frankenterm.agent_compatibility_matrix.v1";

/// Per-agent/version certification status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCompatibilityStatus {
    /// All declared evidence for the relevant strict mode is present.
    Certified,
    /// Some capability evidence exists, but at least one strict requirement is
    /// missing or stale.
    Partial,
    /// Certification ran and found an incompatibility.
    Failed,
    /// Receipt is retained for audit but too old to authorize strict behavior.
    Stale,
}

impl AgentCompatibilityStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Certified => "certified",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }
}

/// Status for a single certification capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentCapabilityStatus {
    /// Evidence proves this capability for the agent/version pair.
    Certified,
    /// Evidence proves this capability is absent or broken.
    Missing,
    /// Certification has not tested this capability yet.
    #[default]
    Unknown,
    /// Capability is not meaningful for this agent/version pair.
    NotApplicable,
}

impl AgentCapabilityStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Certified => "certified",
            Self::Missing => "missing",
            Self::Unknown => "unknown",
            Self::NotApplicable => "not_applicable",
        }
    }

    #[must_use]
    pub const fn satisfies_strict_requirement(self) -> bool {
        matches!(self, Self::Certified)
    }
}

/// Capability dimensions that the certification matrix understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCompatibilityRequirement {
    ComposerDetection,
    SecondEnterBehavior,
    QueuedState,
    RateLimitResetParsing,
    NativeResume,
    Osc133,
    PromptDetection,
    WorkingDetection,
    StuckDetection,
    RedactionSensitiveFixtures,
}

impl AgentCompatibilityRequirement {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ComposerDetection => "composer_detection",
            Self::SecondEnterBehavior => "second_enter_behavior",
            Self::QueuedState => "queued_state",
            Self::RateLimitResetParsing => "rate_limit_reset_parsing",
            Self::NativeResume => "native_resume",
            Self::Osc133 => "osc_133",
            Self::PromptDetection => "prompt_detection",
            Self::WorkingDetection => "working_detection",
            Self::StuckDetection => "stuck_detection",
            Self::RedactionSensitiveFixtures => "redaction_sensitive_fixtures",
        }
    }
}

/// Strict behavior families that must be backed by certification evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStrictMode {
    VerifiedSubmit,
    DurableResume,
    RateLimitAware,
    Osc133Required,
    RedactionSensitiveFixtures,
}

impl AgentStrictMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VerifiedSubmit => "verified_submit",
            Self::DurableResume => "durable_resume",
            Self::RateLimitAware => "rate_limit_aware",
            Self::Osc133Required => "osc_133_required",
            Self::RedactionSensitiveFixtures => "redaction_sensitive_fixtures",
        }
    }

    #[must_use]
    pub const fn requirements(self) -> &'static [AgentCompatibilityRequirement] {
        match self {
            Self::VerifiedSubmit => &[
                AgentCompatibilityRequirement::ComposerDetection,
                AgentCompatibilityRequirement::SecondEnterBehavior,
                AgentCompatibilityRequirement::QueuedState,
                AgentCompatibilityRequirement::PromptDetection,
                AgentCompatibilityRequirement::WorkingDetection,
                AgentCompatibilityRequirement::StuckDetection,
                AgentCompatibilityRequirement::RedactionSensitiveFixtures,
            ],
            Self::DurableResume => &[
                AgentCompatibilityRequirement::NativeResume,
                AgentCompatibilityRequirement::PromptDetection,
                AgentCompatibilityRequirement::WorkingDetection,
                AgentCompatibilityRequirement::StuckDetection,
            ],
            Self::RateLimitAware => &[AgentCompatibilityRequirement::RateLimitResetParsing],
            Self::Osc133Required => &[AgentCompatibilityRequirement::Osc133],
            Self::RedactionSensitiveFixtures => {
                &[AgentCompatibilityRequirement::RedactionSensitiveFixtures]
            }
        }
    }
}

/// Capability verdicts captured by a certification run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentCompatibilityCapabilities {
    #[serde(default)]
    pub composer_detection: AgentCapabilityStatus,
    #[serde(default)]
    pub second_enter_behavior: AgentCapabilityStatus,
    #[serde(default)]
    pub queued_state: AgentCapabilityStatus,
    #[serde(default)]
    pub rate_limit_reset_parsing: AgentCapabilityStatus,
    #[serde(default)]
    pub native_resume: AgentCapabilityStatus,
    #[serde(default)]
    pub osc_133: AgentCapabilityStatus,
    #[serde(default)]
    pub prompt_detection: AgentCapabilityStatus,
    #[serde(default)]
    pub working_detection: AgentCapabilityStatus,
    #[serde(default)]
    pub stuck_detection: AgentCapabilityStatus,
    #[serde(default)]
    pub redaction_sensitive_fixtures: AgentCapabilityStatus,
}

impl AgentCompatibilityCapabilities {
    #[must_use]
    pub const fn status_for(
        &self,
        requirement: AgentCompatibilityRequirement,
    ) -> AgentCapabilityStatus {
        match requirement {
            AgentCompatibilityRequirement::ComposerDetection => self.composer_detection,
            AgentCompatibilityRequirement::SecondEnterBehavior => self.second_enter_behavior,
            AgentCompatibilityRequirement::QueuedState => self.queued_state,
            AgentCompatibilityRequirement::RateLimitResetParsing => self.rate_limit_reset_parsing,
            AgentCompatibilityRequirement::NativeResume => self.native_resume,
            AgentCompatibilityRequirement::Osc133 => self.osc_133,
            AgentCompatibilityRequirement::PromptDetection => self.prompt_detection,
            AgentCompatibilityRequirement::WorkingDetection => self.working_detection,
            AgentCompatibilityRequirement::StuckDetection => self.stuck_detection,
            AgentCompatibilityRequirement::RedactionSensitiveFixtures => {
                self.redaction_sensitive_fixtures
            }
        }
    }
}

/// Evidence summary retained with a certification receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AgentCompatibilityEvidence {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fixture_ids: Vec<String>,
    #[serde(default)]
    pub semantic_query_count: u32,
    #[serde(default)]
    pub submit_profile_id: Option<String>,
    #[serde(default)]
    pub retained_artifact_path: Option<String>,
    #[serde(default)]
    pub redaction_fixture_count: u32,
}

impl AgentCompatibilityEvidence {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fixture_ids.is_empty()
            && self.semantic_query_count == 0
            && self.submit_profile_id.is_none()
            && self.retained_artifact_path.is_none()
            && self.redaction_fixture_count == 0
    }
}

/// Per-agent/version receipt produced by `ft agent certify`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCompatibilityReceipt {
    pub schema_version: u32,
    pub agent_type: AgentType,
    pub agent_version: String,
    pub profile_id: String,
    pub profile_version: String,
    pub certified_at_ms: i64,
    pub status: AgentCompatibilityStatus,
    pub capabilities: AgentCompatibilityCapabilities,
    pub evidence: AgentCompatibilityEvidence,
}

impl AgentCompatibilityReceipt {
    #[must_use]
    pub fn matrix_key(&self) -> AgentCompatibilityMatrixKey {
        AgentCompatibilityMatrixKey {
            agent_type: self.agent_type,
            agent_version: self.agent_version.clone(),
        }
    }

    #[must_use]
    pub fn receipt_id(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(AGENT_COMPATIBILITY_MATRIX_CONTRACT_ID.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.schema_version.to_string().as_bytes());
        hasher.update(b"\n");
        hasher.update(self.agent_type.to_string().as_bytes());
        hasher.update(b"\n");
        hasher.update(self.agent_version.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.profile_id.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.profile_version.as_bytes());
        hasher.update(b"\n");
        hasher.update(self.certified_at_ms.to_string().as_bytes());
        hasher.update(b"\n");
        hasher.update(self.status.as_str().as_bytes());
        for requirement in ALL_AGENT_COMPATIBILITY_REQUIREMENTS {
            let status = self.capabilities.status_for(*requirement);
            hasher.update(b"\n");
            hasher.update(requirement.as_str().as_bytes());
            hasher.update(b"=");
            hasher.update(status.as_str().as_bytes());
        }
        for fixture_id in &self.evidence.fixture_ids {
            hasher.update(b"\nfixture=");
            hasher.update(fixture_id.as_bytes());
        }
        hasher.update(b"\nsemantic_query_count=");
        hasher.update(self.evidence.semantic_query_count.to_string().as_bytes());
        if let Some(submit_profile_id) = &self.evidence.submit_profile_id {
            hasher.update(b"\nsubmit_profile_id=");
            hasher.update(submit_profile_id.as_bytes());
        }
        if let Some(path) = &self.evidence.retained_artifact_path {
            hasher.update(b"\nretained_artifact_path=");
            hasher.update(path.as_bytes());
        }
        hasher.update(b"\nredaction_fixture_count=");
        hasher.update(self.evidence.redaction_fixture_count.to_string().as_bytes());
        format!("agent-compatibility:{}", hex::encode(hasher.finalize()))
    }

    pub fn validate(&self) -> Result<(), AgentCompatibilityValidationError> {
        if self.schema_version != AGENT_COMPATIBILITY_RECEIPT_SCHEMA_VERSION {
            return Err(
                AgentCompatibilityValidationError::UnsupportedSchemaVersion {
                    observed: self.schema_version,
                    expected: AGENT_COMPATIBILITY_RECEIPT_SCHEMA_VERSION,
                },
            );
        }
        if matches!(self.agent_type, AgentType::Unknown | AgentType::Wezterm) {
            return Err(AgentCompatibilityValidationError::UnsupportedAgentType {
                agent_type: self.agent_type,
            });
        }
        if self.agent_version.trim().is_empty() {
            return Err(AgentCompatibilityValidationError::EmptyField {
                field: "agent_version",
            });
        }
        if self.profile_id.trim().is_empty() {
            return Err(AgentCompatibilityValidationError::EmptyField {
                field: "profile_id",
            });
        }
        if self.profile_version.trim().is_empty() {
            return Err(AgentCompatibilityValidationError::EmptyField {
                field: "profile_version",
            });
        }
        if self.certified_at_ms < 0 {
            return Err(AgentCompatibilityValidationError::TimestampNegative {
                field: "certified_at_ms",
                value: self.certified_at_ms,
            });
        }
        if self.evidence.is_empty() {
            return Err(AgentCompatibilityValidationError::MissingEvidence);
        }
        Ok(())
    }

    #[must_use]
    pub fn missing_requirements_for_mode(&self, mode: AgentStrictMode) -> Vec<String> {
        mode.requirements()
            .iter()
            .filter_map(|requirement| {
                let status = self.capabilities.status_for(*requirement);
                if status.satisfies_strict_requirement() {
                    None
                } else {
                    Some(format!("{}:{}", requirement.as_str(), status.as_str()))
                }
            })
            .collect()
    }
}

const ALL_AGENT_COMPATIBILITY_REQUIREMENTS: &[AgentCompatibilityRequirement] = &[
    AgentCompatibilityRequirement::ComposerDetection,
    AgentCompatibilityRequirement::SecondEnterBehavior,
    AgentCompatibilityRequirement::QueuedState,
    AgentCompatibilityRequirement::RateLimitResetParsing,
    AgentCompatibilityRequirement::NativeResume,
    AgentCompatibilityRequirement::Osc133,
    AgentCompatibilityRequirement::PromptDetection,
    AgentCompatibilityRequirement::WorkingDetection,
    AgentCompatibilityRequirement::StuckDetection,
    AgentCompatibilityRequirement::RedactionSensitiveFixtures,
];

/// Query key for a single certified agent/version pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCompatibilityMatrixKey {
    pub agent_type: AgentType,
    pub agent_version: String,
}

impl PartialOrd for AgentCompatibilityMatrixKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AgentCompatibilityMatrixKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        agent_type_order_key(self.agent_type)
            .cmp(&agent_type_order_key(other.agent_type))
            .then_with(|| self.agent_version.cmp(&other.agent_version))
    }
}

const fn agent_type_order_key(agent_type: AgentType) -> u8 {
    match agent_type {
        AgentType::Codex => 0,
        AgentType::ClaudeCode => 1,
        AgentType::Gemini => 2,
        AgentType::Wezterm => 3,
        AgentType::Unknown => 4,
    }
}

/// Queryable certification matrix consumed by strict submit/resume gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCompatibilityMatrix {
    pub schema_version: u32,
    pub contract_id: String,
    pub receipts: Vec<AgentCompatibilityReceipt>,
}

impl Default for AgentCompatibilityMatrix {
    fn default() -> Self {
        Self {
            schema_version: AGENT_COMPATIBILITY_RECEIPT_SCHEMA_VERSION,
            contract_id: AGENT_COMPATIBILITY_MATRIX_CONTRACT_ID.to_string(),
            receipts: Vec::new(),
        }
    }
}

impl AgentCompatibilityMatrix {
    pub fn upsert(
        &mut self,
        receipt: AgentCompatibilityReceipt,
    ) -> Result<Option<AgentCompatibilityReceipt>, AgentCompatibilityValidationError> {
        receipt.validate()?;

        let key = receipt.matrix_key();
        let mut receipts_by_key: BTreeMap<AgentCompatibilityMatrixKey, AgentCompatibilityReceipt> =
            self.receipts
                .drain(..)
                .map(|existing| (existing.matrix_key(), existing))
                .collect();
        let replaced = receipts_by_key.insert(key, receipt);
        self.receipts = receipts_by_key.into_values().collect();
        Ok(replaced)
    }

    #[must_use]
    pub fn get(
        &self,
        agent_type: AgentType,
        agent_version: &str,
    ) -> Option<&AgentCompatibilityReceipt> {
        self.receipts.iter().find(|receipt| {
            receipt.agent_type == agent_type && receipt.agent_version == agent_version
        })
    }

    #[must_use]
    pub fn receipts_for_agent(&self, agent_type: AgentType) -> Vec<&AgentCompatibilityReceipt> {
        self.receipts
            .iter()
            .filter(|receipt| receipt.agent_type == agent_type)
            .collect()
    }

    #[must_use]
    pub fn gate_strict_mode(
        &self,
        agent_type: AgentType,
        agent_version: &str,
        strict_mode: AgentStrictMode,
    ) -> AgentStrictModeGate {
        let Some(receipt) = self.get(agent_type, agent_version) else {
            return AgentStrictModeGate {
                allowed: false,
                agent_type,
                agent_version: agent_version.to_string(),
                strict_mode,
                receipt_id: None,
                reason_codes: vec!["agent_compatibility:uncertified".to_string()],
                missing_requirements: strict_mode
                    .requirements()
                    .iter()
                    .map(|requirement| format!("{}:uncertified", requirement.as_str()))
                    .collect(),
            };
        };

        let mut reason_codes = Vec::new();
        if receipt.status != AgentCompatibilityStatus::Certified {
            reason_codes.push(format!(
                "agent_compatibility:status:{}",
                receipt.status.as_str()
            ));
        }
        let missing_requirements = receipt.missing_requirements_for_mode(strict_mode);
        reason_codes.extend(
            missing_requirements
                .iter()
                .map(|requirement| format!("agent_compatibility:missing:{requirement}")),
        );
        if reason_codes.is_empty() {
            reason_codes.push("agent_compatibility:certified".to_string());
        }

        AgentStrictModeGate {
            allowed: receipt.status == AgentCompatibilityStatus::Certified
                && missing_requirements.is_empty(),
            agent_type,
            agent_version: agent_version.to_string(),
            strict_mode,
            receipt_id: Some(receipt.receipt_id()),
            reason_codes,
            missing_requirements,
        }
    }
}

/// Strict-mode authorization result that workflow/Robot/MCP surfaces can attach
/// to refusals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStrictModeGate {
    pub allowed: bool,
    pub agent_type: AgentType,
    pub agent_version: String,
    pub strict_mode: AgentStrictMode,
    pub receipt_id: Option<String>,
    pub reason_codes: Vec<String>,
    pub missing_requirements: Vec<String>,
}

/// Validation errors for compatibility receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCompatibilityValidationError {
    UnsupportedSchemaVersion { observed: u32, expected: u32 },
    UnsupportedAgentType { agent_type: AgentType },
    EmptyField { field: &'static str },
    TimestampNegative { field: &'static str, value: i64 },
    MissingEvidence,
}

impl std::fmt::Display for AgentCompatibilityValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { observed, expected } => write!(
                f,
                "agent compatibility schema version {observed} is unsupported; expected {expected}"
            ),
            Self::UnsupportedAgentType { agent_type } => {
                write!(f, "agent compatibility cannot certify {agent_type}")
            }
            Self::EmptyField { field } => {
                write!(
                    f,
                    "agent compatibility receipt field {field} must not be empty"
                )
            }
            Self::TimestampNegative { field, value } => {
                write!(
                    f,
                    "agent compatibility receipt {field} is negative: {value}"
                )
            }
            Self::MissingEvidence => write!(
                f,
                "agent compatibility receipt must include at least one evidence source"
            ),
        }
    }
}

impl std::error::Error for AgentCompatibilityValidationError {}

/// Validation error taxonomy. Each variant maps to a specific
/// invariant the handler enforces before persistence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileValidationError {
    NameEmpty,
    NameTooLong {
        len: usize,
        max: usize,
    },
    NameBadChar {
        position: usize,
        observed: char,
    },
    RoleTooLong {
        len: usize,
        max: usize,
    },
    TagEmpty {
        position: usize,
    },
    TagTooLong {
        position: usize,
        len: usize,
        max: usize,
    },
    TooManyTags {
        len: usize,
        max: usize,
    },
    ShellTooLong {
        len: usize,
        max: usize,
    },
    CommandTooLong {
        len: usize,
        max: usize,
    },
    TooManyEnvEntries {
        len: usize,
        max: usize,
    },
    EnvKeyTooLong {
        key_preview: String,
        len: usize,
        max: usize,
    },
    EnvValueTooLong {
        key: String,
        len: usize,
        max: usize,
    },
    TooManyMetadataEntries {
        len: usize,
        max: usize,
    },
    MetadataKeyTooLong {
        key_preview: String,
        len: usize,
        max: usize,
    },
    MetadataValueTooLong {
        key: String,
        len: usize,
        max: usize,
    },
    TimestampNegative {
        field: &'static str,
        value: i64,
    },
    UpdatedBeforeCreated {
        created: i64,
        updated: i64,
    },
}

impl std::fmt::Display for ProfileValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NameEmpty => write!(f, "profile name must not be empty"),
            Self::NameTooLong { len, max } => {
                write!(f, "profile name length {len} exceeds maximum {max}")
            }
            Self::NameBadChar { position, observed } => {
                write!(
                    f,
                    "profile name has invalid character {observed:?} at position {position}; \
                     name must match /^[a-zA-Z0-9_-]+$/"
                )
            }
            Self::RoleTooLong { len, max } => {
                write!(f, "profile role length {len} exceeds maximum {max}")
            }
            Self::TagEmpty { position } => {
                write!(f, "profile tag at position {position} is empty")
            }
            Self::TagTooLong { position, len, max } => {
                write!(
                    f,
                    "profile tag at position {position} has length {len}, exceeds maximum {max}"
                )
            }
            Self::TooManyTags { len, max } => {
                write!(f, "profile has {len} tags, exceeds maximum {max}")
            }
            Self::ShellTooLong { len, max } => {
                write!(f, "profile shell length {len} exceeds maximum {max}")
            }
            Self::CommandTooLong { len, max } => {
                write!(f, "profile command length {len} exceeds maximum {max}")
            }
            Self::TooManyEnvEntries { len, max } => {
                write!(f, "profile has {len} env entries, exceeds maximum {max}")
            }
            Self::EnvKeyTooLong {
                key_preview,
                len,
                max,
            } => {
                write!(
                    f,
                    "profile env key {key_preview:?} (truncated) has length {len}, exceeds maximum {max}"
                )
            }
            Self::EnvValueTooLong { key, len, max } => {
                write!(
                    f,
                    "profile env value for key {key:?} has length {len}, exceeds maximum {max}"
                )
            }
            Self::TooManyMetadataEntries { len, max } => {
                write!(
                    f,
                    "profile has {len} metadata entries, exceeds maximum {max}"
                )
            }
            Self::MetadataKeyTooLong {
                key_preview,
                len,
                max,
            } => {
                write!(
                    f,
                    "profile metadata key {key_preview:?} (truncated) has length {len}, exceeds maximum {max}"
                )
            }
            Self::MetadataValueTooLong { key, len, max } => {
                write!(
                    f,
                    "profile metadata value for key {key:?} has length {len}, exceeds maximum {max}"
                )
            }
            Self::TimestampNegative { field, value } => {
                write!(f, "profile {field} is negative: {value}")
            }
            Self::UpdatedBeforeCreated { created, updated } => {
                write!(
                    f,
                    "profile updated_at_ms ({updated}) precedes created_at_ms ({created})"
                )
            }
        }
    }
}

impl std::error::Error for ProfileValidationError {}

/// Whether `c` is allowed in a profile name (regex: `[a-zA-Z0-9_-]`).
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> AgentProfile {
        AgentProfile {
            name: "cc-agent".to_string(),
            role: "engineer".to_string(),
            tags: vec!["main".to_string(), "default".to_string()],
            shell: "/bin/zsh".to_string(),
            command: Some("claude".to_string()),
            env: {
                let mut m = HashMap::new();
                m.insert(
                    "AGENT_CONFIG_PATH".to_string(),
                    "/tmp/agent-config".to_string(),
                );
                m
            },
            metadata: HashMap::new(),
            created_at_ms: 1_715_000_000_000,
            updated_at_ms: 1_715_000_000_000,
        }
    }

    #[test]
    fn schema_constant_is_well_formed_create_table() {
        assert!(AGENT_PROFILES_SCHEMA.starts_with("CREATE TABLE"));
        assert!(AGENT_PROFILES_SCHEMA.contains("agent_profiles"));
        assert!(AGENT_PROFILES_SCHEMA.contains("name"));
        assert!(AGENT_PROFILES_SCHEMA.contains("role"));
        assert!(AGENT_PROFILES_SCHEMA.contains("tags"));
        assert!(AGENT_PROFILES_SCHEMA.contains("created_at_ms"));
        assert!(AGENT_PROFILES_SCHEMA.contains("updated_at_ms"));
    }

    #[test]
    fn schema_uses_if_not_exists_for_idempotent_migration() {
        // The migration runner re-runs the schema on every
        // launch; IF NOT EXISTS makes that safe.
        assert!(AGENT_PROFILES_SCHEMA.contains("IF NOT EXISTS"));
        assert!(AGENT_PROFILES_ROLE_INDEX.contains("IF NOT EXISTS"));
    }

    #[test]
    fn role_index_targets_role_column() {
        assert!(AGENT_PROFILES_ROLE_INDEX.contains("agent_profiles(role)"));
    }

    #[test]
    fn schema_columns_match_agent_profile_struct_fields() {
        // Schema-vs-struct round-trip pin: every AgentProfile
        // field name must appear as a column in
        // AGENT_PROFILES_SCHEMA. Schema drift catches a future
        // field added to the struct but forgotten in the SQL.
        let schema = AGENT_PROFILES_SCHEMA;
        for field in [
            "name",
            "role",
            "tags",
            "shell",
            "command",
            "env",
            "metadata",
            "created_at_ms",
            "updated_at_ms",
        ] {
            assert!(
                schema.contains(field),
                "AGENT_PROFILES_SCHEMA missing column for AgentProfile.{field}"
            );
        }
    }

    #[test]
    fn validate_rejects_too_many_tags() {
        let mut p = sample_profile();
        p.tags = (0..100).map(|i| format!("t{i}")).collect();
        assert!(p.tags.len() > TAGS_MAX_COUNT);
        assert_eq!(
            p.validate(),
            Err(ProfileValidationError::TooManyTags {
                len: p.tags.len(),
                max: TAGS_MAX_COUNT,
            })
        );
    }

    #[test]
    fn validate_rejects_oversized_shell() {
        let mut p = sample_profile();
        p.shell = "/".repeat(SHELL_MAX_LEN + 1);
        match p.validate() {
            Err(ProfileValidationError::ShellTooLong { len, max }) => {
                assert_eq!(len, SHELL_MAX_LEN + 1);
                assert_eq!(max, SHELL_MAX_LEN);
            }
            other => assert!(false, "expected ShellTooLong; got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_oversized_command() {
        let mut p = sample_profile();
        p.command = Some("x".repeat(COMMAND_MAX_LEN + 1));
        match p.validate() {
            Err(ProfileValidationError::CommandTooLong { len, max }) => {
                assert_eq!(len, COMMAND_MAX_LEN + 1);
                assert_eq!(max, COMMAND_MAX_LEN);
            }
            other => assert!(false, "expected CommandTooLong; got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_too_many_env_entries() {
        let mut p = sample_profile();
        p.env = (0..(ENV_MAX_COUNT + 10))
            .map(|i| (format!("K{i}"), format!("v{i}")))
            .collect();
        match p.validate() {
            Err(ProfileValidationError::TooManyEnvEntries { len, max }) => {
                assert_eq!(len, ENV_MAX_COUNT + 10);
                assert_eq!(max, ENV_MAX_COUNT);
            }
            other => assert!(false, "expected TooManyEnvEntries; got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_oversized_env_value() {
        let mut p = sample_profile();
        p.env
            .insert("K".to_string(), "x".repeat(ENV_VALUE_MAX_LEN + 1));
        match p.validate() {
            Err(ProfileValidationError::EnvValueTooLong { key, len, max }) => {
                assert_eq!(key, "K");
                assert_eq!(len, ENV_VALUE_MAX_LEN + 1);
                assert_eq!(max, ENV_VALUE_MAX_LEN);
            }
            other => assert!(false, "expected EnvValueTooLong; got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_oversized_metadata_entries() {
        let mut p = sample_profile();
        p.metadata = (0..(METADATA_MAX_COUNT + 5))
            .map(|i| (format!("K{i}"), format!("v{i}")))
            .collect();
        match p.validate() {
            Err(ProfileValidationError::TooManyMetadataEntries { len, max }) => {
                assert_eq!(len, METADATA_MAX_COUNT + 5);
                assert_eq!(max, METADATA_MAX_COUNT);
            }
            other => assert!(false, "expected TooManyMetadataEntries; got {other:?}"),
        }
    }

    #[test]
    fn validate_accepts_a_well_formed_profile() {
        let p = sample_profile();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_empty_name() {
        let mut p = sample_profile();
        p.name = String::new();
        assert_eq!(p.validate(), Err(ProfileValidationError::NameEmpty));
    }

    #[test]
    fn validate_rejects_name_over_max_length() {
        let mut p = sample_profile();
        p.name = "a".repeat(NAME_MAX_LEN + 1);
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_name_with_bad_char() {
        let mut p = sample_profile();
        p.name = "bad name".to_string(); // space not allowed
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameBadChar { observed: ' ', .. })
        ));
    }

    #[test]
    fn validate_accepts_alphanumeric_underscore_hyphen() {
        let names = [
            "simple",
            "with_underscore",
            "with-hyphen",
            "Mixed123",
            "all-the_characters-1234",
        ];
        for name in &names {
            let mut p = sample_profile();
            p.name = (*name).to_string();
            assert_eq!(p.validate(), Ok(()), "name {name:?} should validate");
        }
    }

    #[test]
    fn validate_rejects_role_over_max_length() {
        let mut p = sample_profile();
        p.role = "x".repeat(ROLE_MAX_LEN + 1);
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::RoleTooLong { .. })
        ));
    }

    #[test]
    fn validate_accepts_empty_role() {
        let mut p = sample_profile();
        p.role = String::new();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_empty_tag() {
        let mut p = sample_profile();
        p.tags = vec!["good".to_string(), String::new(), "also_good".to_string()];
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::TagEmpty { position: 1 })
        ));
    }

    #[test]
    fn validate_rejects_oversize_tag() {
        let mut p = sample_profile();
        p.tags = vec!["x".repeat(TAG_MAX_LEN + 1)];
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::TagTooLong { .. })
        ));
    }

    #[test]
    fn validate_rejects_negative_timestamps() {
        let mut p = sample_profile();
        p.created_at_ms = -1;
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::TimestampNegative {
                field: "created_at_ms",
                ..
            })
        ));
    }

    #[test]
    fn validate_rejects_updated_before_created() {
        let mut p = sample_profile();
        p.created_at_ms = 1000;
        p.updated_at_ms = 500;
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::UpdatedBeforeCreated { .. })
        ));
    }

    #[test]
    fn validation_error_renders_each_variant() {
        let errs = vec![
            ProfileValidationError::NameEmpty,
            ProfileValidationError::NameTooLong { len: 100, max: 64 },
            ProfileValidationError::NameBadChar {
                position: 3,
                observed: '!',
            },
            ProfileValidationError::RoleTooLong { len: 100, max: 64 },
            ProfileValidationError::TagEmpty { position: 0 },
            ProfileValidationError::TagTooLong {
                position: 0,
                len: 100,
                max: 64,
            },
            ProfileValidationError::TimestampNegative {
                field: "created_at_ms",
                value: -1,
            },
            ProfileValidationError::UpdatedBeforeCreated {
                created: 1000,
                updated: 500,
            },
        ];
        for err in &errs {
            let s = err.to_string();
            assert!(!s.is_empty(), "Display for {err:?} produced empty string");
        }
    }

    #[test]
    fn serde_round_trips_a_profile() {
        let p = sample_profile();
        let json = match serde_json::to_string(&p) {
            Ok(json) => json,
            Err(err) => {
                assert!(false, "profile serialization failed: {err}");
                return;
            }
        };
        let decoded: AgentProfile = match serde_json::from_str(&json) {
            Ok(decoded) => decoded,
            Err(err) => {
                assert!(false, "profile deserialization failed: {err}");
                return;
            }
        };
        assert_eq!(p, decoded);
    }

    #[test]
    fn serde_defaults_apply_when_optional_fields_are_omitted() {
        // Minimal JSON — only required fields.
        let json = r#"{
            "name": "minimal",
            "created_at_ms": 0,
            "updated_at_ms": 0
        }"#;
        let p: AgentProfile = match serde_json::from_str(json) {
            Ok(profile) => profile,
            Err(err) => {
                assert!(false, "minimal profile deserialization failed: {err}");
                return;
            }
        };
        assert_eq!(p.name, "minimal");
        assert_eq!(p.role, "");
        assert!(p.tags.is_empty());
        assert_eq!(p.shell, "");
        assert_eq!(p.command, None);
        assert!(p.env.is_empty());
        assert!(p.metadata.is_empty());
    }

    // ========================================================================
    // ft-730id: boundary + edge-case coverage
    // ========================================================================

    #[test]
    fn validate_accepts_name_at_exactly_max_len() {
        let mut p = sample_profile();
        p.name = "a".repeat(NAME_MAX_LEN);
        assert_eq!(
            p.name.len(),
            NAME_MAX_LEN,
            "test self-check: name should be at exactly NAME_MAX_LEN bytes"
        );
        assert_eq!(
            p.validate(),
            Ok(()),
            "off-by-one at boundary: == max_len must be accepted"
        );
    }

    #[test]
    fn validate_accepts_role_at_exactly_max_len() {
        let mut p = sample_profile();
        p.role = "x".repeat(ROLE_MAX_LEN);
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_tag_at_exactly_max_len() {
        let mut p = sample_profile();
        p.tags = vec!["t".repeat(TAG_MAX_LEN)];
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_rejects_name_with_non_ascii_chars() {
        // is_name_char uses is_ascii_alphanumeric — rejects unicode
        // letters. Pin this contract: operators using locale-
        // specific characters get a clear error rather than silent
        // truncation or surprising acceptance.
        let mut p = sample_profile();
        p.name = "café".to_string();
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameBadChar { observed, .. }) if observed == 'é'
        ));
    }

    #[test]
    fn validate_name_length_is_byte_count_not_char_count() {
        // name.len() returns BYTES, not chars. A name composed of
        // multi-byte UTF-8 chars consumes more of the budget than
        // its char count suggests. (The chars themselves would be
        // rejected by is_name_char, but if a future relaxation
        // permits unicode, this test pins the byte-count semantic.)
        let mut p = sample_profile();
        // 32 'é' chars = 64 UTF-8 bytes (each 'é' is 2 bytes).
        // At byte-level this would be exactly NAME_MAX_LEN, but
        // each char is non-ASCII so NameBadChar fires first.
        p.name = "é".repeat(32);
        assert_eq!(
            p.name.len(),
            64,
            "multi-byte chars: 32 chars × 2 bytes = 64"
        );
        // The bad-char error fires before length is even checked
        // (current order: emptiness → length → charset).
        // Specifically: length 64 is == max so length passes; bad
        // char fires.
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameBadChar { .. })
        ));
    }

    #[test]
    fn validate_error_order_empty_takes_precedence_over_length() {
        // When name is empty, NameEmpty fires before any other
        // check. Pin the validation order so callers don't get a
        // surprise from a silent reordering.
        let mut p = sample_profile();
        p.name = String::new();
        // Even if other invariants would fire, NameEmpty wins.
        p.role = "x".repeat(ROLE_MAX_LEN + 1);
        p.tags = vec![String::new()]; // empty tag
        assert_eq!(p.validate(), Err(ProfileValidationError::NameEmpty));
    }

    #[test]
    fn validate_error_order_name_length_takes_precedence_over_charset() {
        // After NameEmpty: NameTooLong before NameBadChar. A name
        // that's both oversized AND has a bad char should report
        // the length error first.
        let mut p = sample_profile();
        // 65 chars, mix of ASCII and bad chars at the front.
        p.name = " ".to_string() + &"a".repeat(NAME_MAX_LEN);
        assert_eq!(p.name.len(), NAME_MAX_LEN + 1);
        // Length check fires before charset check.
        assert!(matches!(
            p.validate(),
            Err(ProfileValidationError::NameTooLong { .. })
        ));
    }

    #[test]
    fn validate_accepts_empty_tags_vec() {
        // No tags = trivially valid (no element to fail).
        let mut p = sample_profile();
        p.tags = Vec::new();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_zero_zero_timestamps() {
        // created_at_ms = updated_at_ms = 0 is valid (both zero,
        // updated >= created via equality). Pin this so the bead's
        // "non-negative timestamps" contract is unambiguous at the
        // boundary.
        let mut p = sample_profile();
        p.created_at_ms = 0;
        p.updated_at_ms = 0;
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_accepts_equal_timestamps() {
        // updated == created passes (the check is updated <
        // created, not strictly greater).
        let mut p = sample_profile();
        p.created_at_ms = 1_715_000_000_000;
        p.updated_at_ms = 1_715_000_000_000;
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn schema_columns_appear_in_documented_order() {
        // The migration runner depends on the column order being
        // stable. Pin it explicitly so a future "alphabetize the
        // columns" cleanup doesn't silently break a follow-on
        // migration that relies on the current order.
        let columns_in_order = [
            "name",
            "role",
            "tags",
            "shell",
            "command",
            "env",
            "metadata",
            "created_at_ms",
            "updated_at_ms",
        ];
        let mut last_pos = 0;
        for col in &columns_in_order {
            let Some(pos) = AGENT_PROFILES_SCHEMA.find(col) else {
                assert!(false, "column {col} missing from schema");
                return;
            };
            assert!(
                pos >= last_pos,
                "column {col} (pos {pos}) appears before previous column (pos {last_pos}) — order drifted"
            );
            last_pos = pos;
        }
    }

    #[test]
    fn validate_accepts_tag_with_unicode_content() {
        // Tags currently have no charset restriction (unlike name).
        // A tag with unicode / emoji content is valid as long as
        // it's non-empty and within byte-length budget. Pin this
        // contract.
        let mut p = sample_profile();
        p.tags = vec!["🚀".to_string(), "café".to_string(), "tag-123".to_string()];
        assert_eq!(
            p.validate(),
            Ok(()),
            "tags accept arbitrary UTF-8 content (no charset restriction)"
        );
    }

    fn certified_codex_receipt(version: &str) -> AgentCompatibilityReceipt {
        AgentCompatibilityReceipt {
            schema_version: AGENT_COMPATIBILITY_RECEIPT_SCHEMA_VERSION,
            agent_type: AgentType::Codex,
            agent_version: version.to_string(),
            profile_id: "codex.default".to_string(),
            profile_version: "2026-06-14".to_string(),
            certified_at_ms: 1_797_200_000_000,
            status: AgentCompatibilityStatus::Certified,
            capabilities: AgentCompatibilityCapabilities {
                composer_detection: AgentCapabilityStatus::Certified,
                second_enter_behavior: AgentCapabilityStatus::Certified,
                queued_state: AgentCapabilityStatus::Certified,
                rate_limit_reset_parsing: AgentCapabilityStatus::Certified,
                native_resume: AgentCapabilityStatus::Certified,
                osc_133: AgentCapabilityStatus::Missing,
                prompt_detection: AgentCapabilityStatus::Certified,
                working_detection: AgentCapabilityStatus::Certified,
                stuck_detection: AgentCapabilityStatus::Certified,
                redaction_sensitive_fixtures: AgentCapabilityStatus::Certified,
            },
            evidence: AgentCompatibilityEvidence {
                fixture_ids: vec![
                    "codex-composer-basic".to_string(),
                    "codex-redaction-sensitive".to_string(),
                ],
                semantic_query_count: 4,
                submit_profile_id: Some("codex.default".to_string()),
                retained_artifact_path: Some(
                    "fixtures/agent-compatibility/codex-2026-06-14.jsonl".to_string(),
                ),
                redaction_fixture_count: 1,
            },
        }
    }

    #[test]
    fn compatibility_receipt_serializes_with_stable_golden_shape() {
        let receipt = certified_codex_receipt("0.12.0");
        let value = match serde_json::to_value(&receipt) {
            Ok(value) => value,
            Err(err) => {
                assert!(false, "compatibility receipt serialization failed: {err}");
                return;
            }
        };
        let expected = serde_json::json!({
            "schema_version": 1,
            "agent_type": "codex",
            "agent_version": "0.12.0",
            "profile_id": "codex.default",
            "profile_version": "2026-06-14",
            "certified_at_ms": 1797200000000_i64,
            "status": "certified",
            "capabilities": {
                "composer_detection": "certified",
                "second_enter_behavior": "certified",
                "queued_state": "certified",
                "rate_limit_reset_parsing": "certified",
                "native_resume": "certified",
                "osc_133": "missing",
                "prompt_detection": "certified",
                "working_detection": "certified",
                "stuck_detection": "certified",
                "redaction_sensitive_fixtures": "certified"
            },
            "evidence": {
                "fixture_ids": [
                    "codex-composer-basic",
                    "codex-redaction-sensitive"
                ],
                "semantic_query_count": 4,
                "submit_profile_id": "codex.default",
                "retained_artifact_path": "fixtures/agent-compatibility/codex-2026-06-14.jsonl",
                "redaction_fixture_count": 1
            }
        });
        assert_eq!(value, expected);
    }

    #[test]
    fn compatibility_receipt_validation_rejects_uncertifiable_agents() {
        let mut receipt = certified_codex_receipt("0.12.0");
        receipt.agent_type = AgentType::Wezterm;
        assert_eq!(
            receipt.validate(),
            Err(AgentCompatibilityValidationError::UnsupportedAgentType {
                agent_type: AgentType::Wezterm
            })
        );
    }

    #[test]
    fn compatibility_matrix_upsert_is_queryable_by_agent_and_version() {
        let mut matrix = AgentCompatibilityMatrix::default();
        assert_eq!(matrix.upsert(certified_codex_receipt("0.12.0")), Ok(None));
        assert_eq!(matrix.upsert(certified_codex_receipt("0.13.0")), Ok(None));

        let codex_receipts = matrix.receipts_for_agent(AgentType::Codex);
        assert_eq!(codex_receipts.len(), 2);
        let Some(receipt) = matrix.get(AgentType::Codex, "0.13.0") else {
            assert!(false, "matrix should return codex 0.13.0 receipt");
            return;
        };
        assert_eq!(receipt.profile_id, "codex.default");
    }

    #[test]
    fn strict_verified_submit_gate_allows_certified_receipt() {
        let mut matrix = AgentCompatibilityMatrix::default();
        assert_eq!(matrix.upsert(certified_codex_receipt("0.12.0")), Ok(None));

        let gate =
            matrix.gate_strict_mode(AgentType::Codex, "0.12.0", AgentStrictMode::VerifiedSubmit);
        assert!(gate.allowed, "certified verified-submit gate should pass");
        assert_eq!(
            gate.reason_codes,
            vec!["agent_compatibility:certified".to_string()]
        );
        assert!(gate.receipt_id.is_some());
        assert!(gate.missing_requirements.is_empty());
    }

    #[test]
    fn strict_mode_gate_refuses_uncertified_or_missing_capability() {
        let matrix = AgentCompatibilityMatrix::default();
        let missing_receipt_gate =
            matrix.gate_strict_mode(AgentType::Codex, "0.12.0", AgentStrictMode::VerifiedSubmit);
        assert!(!missing_receipt_gate.allowed);
        assert_eq!(
            missing_receipt_gate.reason_codes,
            vec!["agent_compatibility:uncertified".to_string()]
        );

        let mut partial = certified_codex_receipt("0.12.0");
        partial.capabilities.second_enter_behavior = AgentCapabilityStatus::Missing;
        partial.status = AgentCompatibilityStatus::Partial;
        let mut matrix = AgentCompatibilityMatrix::default();
        assert_eq!(matrix.upsert(partial), Ok(None));

        let gate =
            matrix.gate_strict_mode(AgentType::Codex, "0.12.0", AgentStrictMode::VerifiedSubmit);
        assert!(!gate.allowed);
        assert!(
            gate.reason_codes
                .contains(&"agent_compatibility:status:partial".to_string())
        );
        assert!(
            gate.missing_requirements
                .contains(&"second_enter_behavior:missing".to_string())
        );
    }
}
