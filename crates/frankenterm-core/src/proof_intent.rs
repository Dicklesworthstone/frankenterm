//! `ProofIntent` — a durable, replayable record of a proof that was *intended*
//! but could not be terminally proven yet (typically because the RCH remote
//! lane was unavailable). It preserves the EXACT intent — command, scope, source
//! hash, expected artifact, remote requirement, bead, attestation slot, and
//! redaction policy — so a future agent (or the W8.2 conveyor) can replay it
//! against the same source tree instead of reconstructing it from scrollback.
//!
//! Bead: `ft-7h5da.9.1` (W8.1) of the 2026-06-06 dueling-idea-wizards program.
//! This is METADATA-FIRST: defining the schema only. The queue/replay/admission
//! automation is W8.2; the proof-quality classifier is W8.3.
//!
//! Design notes:
//! - No `#[serde(skip_serializing_if = ...)]` — every field always serializes,
//!   in declaration order, so the struct is safe for positional binary
//!   (varbincode) persistence as well as JSON.
//! - `intent_id` is content-addressed over the binding (command, scope, source
//!   hash, kind, remote requirement, bead, slot) and EXCLUDES `created_at_ms`,
//!   so the same intent reproduced later collapses to one id.
//! - `source_hash` is the staleness key: replay must refuse if the live tree's
//!   hash differs (re-running a stale command against a moved tree is the replay
//!   hazard W8.2 guards against).

#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};

/// Stable contract id for the proof-intent schema.
pub const PROOF_INTENT_CONTRACT_ID: &str = "ft.proof_intent.v1";

/// Current schema version (forward-compatibility guard).
pub const PROOF_INTENT_SCHEMA_VERSION: u32 = 1;

/// Build/proof scope. Package-scoped proofs are cheaper and preferred under
/// pressure; workspace-wide proofs are stronger but heavier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Tag is `type` (not `kind`) to avoid visual confusion with the sibling `kind`
// field on `ProofIntent` once a scope is nested inside it.
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProofScope {
    /// A single package (`cargo ... -p <package>`).
    Package {
        /// The cargo package name.
        package: String,
    },
    /// The whole workspace (`cargo ... --workspace`).
    Workspace,
}

impl ProofScope {
    /// Stable, separator-free token for the content hash.
    #[must_use]
    fn canonical_token(&self) -> String {
        match self {
            Self::Package { package } => format!("package:{package}"),
            Self::Workspace => "workspace".to_string(),
        }
    }
}

/// The class of proof a command produces. Carried on the intent so replay and
/// the W8.3 classifier don't have to re-parse the raw command string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofKind {
    /// `cargo test`
    Test,
    /// `cargo check`
    Check,
    /// `cargo clippy`
    Clippy,
    /// `cargo fmt --check`
    Fmt,
    /// JSON-schema / golden conformance.
    Schema,
    /// `cargo fuzz` / fuzz target run.
    Fuzz,
    /// Replay-harness determinism proof.
    Replay,
    /// Attestation verification (`ft attestation verify`).
    Attestation,
}

impl ProofKind {
    /// Stable string token (matches the serde representation). Used for the
    /// content hash so the id never depends on `Debug` formatting.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Check => "check",
            Self::Clippy => "clippy",
            Self::Fmt => "fmt",
            Self::Schema => "schema",
            Self::Fuzz => "fuzz",
            Self::Replay => "replay",
            Self::Attestation => "attestation",
        }
    }
}

/// Redaction policy applied to any captured proof output before it is persisted
/// or surfaced. Mirrors the project's T1/T2/T3 sensitivity tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProofRedactionPolicy {
    /// Standard read-path redaction (default).
    #[default]
    Standard,
    /// Most aggressive tier — for proofs over secret-bearing surfaces.
    Strict,
}

impl ProofRedactionPolicy {
    /// Stable string token (matches the serde representation) for the hash.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Strict => "strict",
        }
    }
}

/// Reasons a [`ProofIntent`] fails validation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProofIntentError {
    /// The intent was produced by a newer schema than this binary supports.
    #[error("proof intent schema_version {found} exceeds max supported {max_supported}")]
    UnsupportedSchemaVersion {
        /// Schema version found on the intent.
        found: u32,
        /// Highest schema version this binary understands.
        max_supported: u32,
    },
    /// The command string is empty — nothing to replay.
    #[error("proof intent has an empty command")]
    EmptyCommand,
    /// The source hash is empty — staleness can never be checked.
    #[error("proof intent has an empty source_hash (staleness uncheckable)")]
    EmptySourceHash,
}

/// A durable record of an intended-but-deferred proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofIntent {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// Content-addressed id (`proof:<32 hex>`), derived from the binding.
    pub intent_id: String,
    /// The exact command to (re)run, e.g. the full `rch ... cargo test ...` line.
    pub command: String,
    /// Package vs workspace scope.
    pub scope: ProofScope,
    /// The class of proof this command produces.
    pub kind: ProofKind,
    /// Source-tree hash captured at intent time; the staleness key for replay.
    pub source_hash: String,
    /// Where the proof artifact is expected to land (if any).
    pub expected_artifact_path: Option<String>,
    /// Whether this proof MUST run on a remote RCH worker (no local fallback).
    pub required_remote: bool,
    /// The bead this proof closes/supports, if any.
    pub bead_id: Option<String>,
    /// The attestation slot this proof feeds, if any.
    pub attestation_slot: Option<String>,
    /// Redaction policy for captured output.
    pub redaction_policy: ProofRedactionPolicy,
    /// Creation time (epoch ms); excluded from `intent_id`.
    pub created_at_ms: i64,
}

impl ProofIntent {
    /// Build a proof intent, computing the content-addressed `intent_id`.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        command: impl Into<String>,
        scope: ProofScope,
        kind: ProofKind,
        source_hash: impl Into<String>,
        expected_artifact_path: Option<String>,
        required_remote: bool,
        bead_id: Option<String>,
        attestation_slot: Option<String>,
        redaction_policy: ProofRedactionPolicy,
        created_at_ms: i64,
    ) -> Self {
        let mut intent = Self {
            schema_version: PROOF_INTENT_SCHEMA_VERSION,
            intent_id: String::new(),
            command: command.into(),
            scope,
            kind,
            source_hash: source_hash.into(),
            expected_artifact_path,
            required_remote,
            bead_id,
            attestation_slot,
            redaction_policy,
            created_at_ms,
        };
        intent.intent_id = intent.compute_id();
        intent
    }

    /// Deterministic canonical string used to derive [`Self::intent_id`].
    /// Excludes `intent_id` and `created_at_ms`.
    #[must_use]
    pub fn canonical_string(&self) -> String {
        format!(
            "v={};cmd={};scope={};kind={};src={};artifact={};remote={};bead={};slot={};redact={}",
            self.schema_version,
            self.command,
            self.scope.canonical_token(),
            self.kind.as_str(),
            self.source_hash,
            self.expected_artifact_path.as_deref().unwrap_or("none"),
            self.required_remote,
            self.bead_id.as_deref().unwrap_or("none"),
            self.attestation_slot.as_deref().unwrap_or("none"),
            self.redaction_policy.as_str(),
        )
    }

    /// Compute the content-addressed intent id from [`Self::canonical_string`].
    #[must_use]
    pub fn compute_id(&self) -> String {
        let hash = crate::replay_capture::sha256_hex(&self.canonical_string());
        format!("proof:{}", &hash[..32])
    }

    /// Whether the live source tree has drifted from when this intent was
    /// captured. A `true` here means replay must REFUSE (the command would run
    /// against a different tree than intended).
    #[must_use]
    pub fn is_stale(&self, live_source_hash: &str) -> bool {
        self.source_hash.as_str() != live_source_hash
    }

    /// Forward-compatibility + invariant guard. Call before queueing or
    /// replaying an intent loaded from disk or the wire.
    ///
    /// # Errors
    /// Returns [`ProofIntentError`] if the schema is too new, the command is
    /// empty, or the source hash is empty.
    pub fn validate(&self) -> Result<(), ProofIntentError> {
        if self.schema_version > PROOF_INTENT_SCHEMA_VERSION {
            return Err(ProofIntentError::UnsupportedSchemaVersion {
                found: self.schema_version,
                max_supported: PROOF_INTENT_SCHEMA_VERSION,
            });
        }
        if self.command.trim().is_empty() {
            return Err(ProofIntentError::EmptyCommand);
        }
        if self.source_hash.trim().is_empty() {
            return Err(ProofIntentError::EmptySourceHash);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(source_hash: &str, created_at_ms: i64) -> ProofIntent {
        ProofIntent::new(
            "RCH_REQUIRE_REMOTE=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/x cargo test -p frankenterm-core --lib steering",
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            ProofKind::Test,
            source_hash,
            Some("target/test-logs/steering/result.jsonl".to_string()),
            true,
            Some("ft-7h5da.6.1".to_string()),
            None,
            ProofRedactionPolicy::Standard,
            created_at_ms,
        )
    }

    #[test]
    fn builds_with_content_addressed_id() {
        let p = sample("sha256:tree-aaaa", 1_704_000_000_000);
        assert_eq!(p.schema_version, PROOF_INTENT_SCHEMA_VERSION);
        assert!(p.intent_id.starts_with("proof:"));
        assert_eq!(p.intent_id.len(), "proof:".len() + 32);
        assert!(p.required_remote);
    }

    #[test]
    fn intent_id_excludes_created_at() {
        let a = sample("sha256:tree-aaaa", 1_704_000_000_000);
        let b = sample("sha256:tree-aaaa", 1_999_999_999_999);
        assert_eq!(
            a.intent_id, b.intent_id,
            "intent_id must exclude created_at_ms"
        );
    }

    #[test]
    fn distinct_source_hash_changes_id() {
        let a = sample("sha256:tree-aaaa", 1_704_000_000_000);
        let b = sample("sha256:tree-bbbb", 1_704_000_000_000);
        assert_ne!(
            a.intent_id, b.intent_id,
            "different source trees are different intents"
        );
    }

    #[test]
    fn staleness_detection() {
        let p = sample("sha256:tree-aaaa", 1_704_000_000_000);
        assert!(!p.is_stale("sha256:tree-aaaa"), "same tree is not stale");
        assert!(
            p.is_stale("sha256:tree-cccc"),
            "moved tree is stale (replay must refuse)"
        );
    }

    #[test]
    fn json_round_trip_is_lossless() {
        let p = sample("sha256:tree-aaaa", 1_704_000_000_000);
        let json = serde_json::to_string(&p).expect("serialize");
        let back: ProofIntent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(p, back);
    }

    #[test]
    fn scope_variants_round_trip() {
        let ws = ProofIntent::new(
            "cargo test --workspace",
            ProofScope::Workspace,
            ProofKind::Test,
            "sha256:tree-aaaa",
            None,
            true,
            None,
            None,
            ProofRedactionPolicy::Strict,
            1_704_000_000_000,
        );
        let json = serde_json::to_string(&ws).expect("serialize");
        assert!(
            json.contains("\"type\":\"workspace\""),
            "scope tag present: {json}"
        );
        // The top-level proof kind still serializes under `kind` (no collision
        // with the scope discriminator, which is now `type`).
        assert!(json.contains("\"kind\":\"test\""), "proof kind present: {json}");
        let back: ProofIntent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ws, back);
        assert_ne!(
            ws.intent_id,
            sample("sha256:tree-aaaa", 0).intent_id,
            "scope affects id"
        );
    }

    #[test]
    fn validate_rejects_future_schema_empty_command_and_empty_source() {
        let mut p = sample("sha256:tree-aaaa", 1_704_000_000_000);
        assert!(p.validate().is_ok());

        p.schema_version = PROOF_INTENT_SCHEMA_VERSION + 1;
        assert!(matches!(
            p.validate(),
            Err(ProofIntentError::UnsupportedSchemaVersion { .. })
        ));

        p.schema_version = PROOF_INTENT_SCHEMA_VERSION;
        p.command = "   ".to_string();
        assert!(matches!(p.validate(), Err(ProofIntentError::EmptyCommand)));

        p.command = "cargo test".to_string();
        p.source_hash = String::new();
        assert!(matches!(
            p.validate(),
            Err(ProofIntentError::EmptySourceHash)
        ));
    }

    #[test]
    fn redaction_policy_defaults_to_standard() {
        assert_eq!(
            ProofRedactionPolicy::default(),
            ProofRedactionPolicy::Standard
        );
    }
}
