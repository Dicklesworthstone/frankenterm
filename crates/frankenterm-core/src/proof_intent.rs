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

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Stable contract id for the proof-intent schema.
pub const PROOF_INTENT_CONTRACT_ID: &str = "ft.proof_intent.v1";

/// Current schema version (forward-compatibility guard).
pub const PROOF_INTENT_SCHEMA_VERSION: u32 = 1;

/// Stable contract id for the durable proof-intent queue.
pub const PROOF_INTENT_QUEUE_ENTRY_CONTRACT_ID: &str = "ft.proof_intent_queue_entry.v1";

/// Current proof-intent queue schema version.
pub const PROOF_INTENT_QUEUE_SCHEMA_VERSION: u32 = 1;

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
    /// Stable, length-delimited token for the content hash.
    #[must_use]
    fn canonical_token(&self) -> String {
        match self {
            Self::Package { package } => format!("package({})", canonical_value(package)),
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
    /// The stored id does not match the current content binding.
    #[error("proof intent id mismatch: found {found}, expected {expected}")]
    IntentIdMismatch {
        /// Stored id found on the intent.
        found: String,
        /// Id recomputed from the current binding fields.
        expected: String,
    },
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
        [
            canonical_field("schema_version", &self.schema_version.to_string()),
            canonical_field("command", &self.command),
            canonical_field("scope", &self.scope.canonical_token()),
            canonical_field("kind", self.kind.as_str()),
            canonical_field("source_hash", &self.source_hash),
            canonical_option_field(
                "expected_artifact_path",
                self.expected_artifact_path.as_deref(),
            ),
            canonical_field("required_remote", &self.required_remote.to_string()),
            canonical_option_field("bead_id", self.bead_id.as_deref()),
            canonical_option_field("attestation_slot", self.attestation_slot.as_deref()),
            canonical_field("redaction_policy", self.redaction_policy.as_str()),
        ]
        .join("|")
    }

    /// Compute the content-addressed intent id from [`Self::canonical_string`].
    #[must_use]
    pub fn compute_id(&self) -> String {
        let hash = sha256_hex(&self.canonical_string());
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
    /// empty, the source hash is empty, or the stored id does not match the
    /// current content binding.
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
        let expected = self.compute_id();
        if self.intent_id != expected {
            return Err(ProofIntentError::IntentIdMismatch {
                found: self.intent_id.clone(),
                expected,
            });
        }
        Ok(())
    }
}

/// Environment variable captured outside the argv vector for exact replay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofIntentEnvVar {
    /// Environment variable name.
    pub name: String,
    /// Environment variable value.
    pub value: String,
}

/// Retained attempt metadata attached to a queued proof intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofIntentReplayAttemptRef {
    /// Retained attempt record path, when a live replay wrote one.
    pub attempt_record_path: Option<String>,
    /// Terminal replay outcome string.
    pub outcome: String,
    /// Whether the replay reached a remote worker/Cargo lane.
    pub remote_cargo_reached: bool,
    /// Whether RCH local fallback was detected.
    pub local_fallback_detected: bool,
    /// Recording timestamp in epoch milliseconds.
    pub recorded_at_ms: i64,
}

/// A proof receipt explicitly attached to a queued intent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofIntentAttachedReceipt {
    /// Path to the retained proof receipt or attempt record.
    pub receipt_path: String,
    /// Attachment timestamp in epoch milliseconds.
    pub attached_at_ms: i64,
}

/// Durable JSONL row for W8.2's deferred proof queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofIntentQueueEntry {
    /// Queue schema version.
    pub schema_version: u32,
    /// Queue contract id.
    pub contract_id: String,
    /// Content-addressed proof intent.
    pub intent: ProofIntent,
    /// Exact command argv after leading environment assignments are stripped.
    pub command_argv: Vec<String>,
    /// Leading environment assignments required for replay.
    pub command_env: Vec<ProofIntentEnvVar>,
    /// Cargo target directory, when known.
    pub target_dir: Option<String>,
    /// Latest RCH/operating-envelope admission state for this intent.
    pub rch_admission_state: String,
    /// Queue timestamp in epoch milliseconds.
    pub queued_at_ms: i64,
    /// Last mutation timestamp in epoch milliseconds.
    pub updated_at_ms: i64,
    /// Retained replay attempts.
    #[serde(default)]
    pub replay_attempts: Vec<ProofIntentReplayAttemptRef>,
    /// Explicitly attached proof receipts.
    #[serde(default)]
    pub attached_receipts: Vec<ProofIntentAttachedReceipt>,
}

impl ProofIntentQueueEntry {
    /// Build a durable queue entry around a validated [`ProofIntent`].
    #[must_use]
    pub fn new(
        intent: ProofIntent,
        command_argv: Vec<String>,
        command_env: Vec<ProofIntentEnvVar>,
        target_dir: Option<String>,
        rch_admission_state: impl Into<String>,
        queued_at_ms: i64,
    ) -> Self {
        Self {
            schema_version: PROOF_INTENT_QUEUE_SCHEMA_VERSION,
            contract_id: PROOF_INTENT_QUEUE_ENTRY_CONTRACT_ID.to_string(),
            intent,
            command_argv,
            command_env,
            target_dir,
            rch_admission_state: rch_admission_state.into(),
            queued_at_ms,
            updated_at_ms: queued_at_ms,
            replay_attempts: Vec::new(),
            attached_receipts: Vec::new(),
        }
    }

    /// Validate queue-row invariants before persisting or replaying.
    ///
    /// # Errors
    /// Returns [`ProofIntentQueueError`] if the queue row is from a newer
    /// schema, has the wrong contract, has no argv, has no admission state, or
    /// wraps an invalid [`ProofIntent`].
    pub fn validate(&self) -> Result<(), ProofIntentQueueError> {
        if self.schema_version > PROOF_INTENT_QUEUE_SCHEMA_VERSION {
            return Err(ProofIntentQueueError::InvalidEntry {
                intent_id: self.intent.intent_id.clone(),
                reason: format!(
                    "queue schema_version {} exceeds max supported {}",
                    self.schema_version, PROOF_INTENT_QUEUE_SCHEMA_VERSION
                ),
            });
        }
        if self.contract_id != PROOF_INTENT_QUEUE_ENTRY_CONTRACT_ID {
            return Err(ProofIntentQueueError::InvalidEntry {
                intent_id: self.intent.intent_id.clone(),
                reason: format!("unexpected queue contract_id {}", self.contract_id),
            });
        }
        self.intent
            .validate()
            .map_err(|error| ProofIntentQueueError::InvalidEntry {
                intent_id: self.intent.intent_id.clone(),
                reason: error.to_string(),
            })?;
        if self.command_argv.is_empty() {
            return Err(ProofIntentQueueError::InvalidEntry {
                intent_id: self.intent.intent_id.clone(),
                reason: "queued proof command argv is empty".to_string(),
            });
        }
        if self.rch_admission_state.trim().is_empty() {
            return Err(ProofIntentQueueError::InvalidEntry {
                intent_id: self.intent.intent_id.clone(),
                reason: "rch_admission_state is empty".to_string(),
            });
        }
        Ok(())
    }

    /// True when the live tree hash no longer matches the queued source hash.
    #[must_use]
    pub fn is_stale(&self, live_source_hash: &str) -> bool {
        self.intent.is_stale(live_source_hash)
    }

    /// Append retained live-replay metadata to this queue entry.
    pub fn record_replay_attempt(
        &mut self,
        attempt: ProofIntentReplayAttemptRef,
        updated_at_ms: i64,
    ) {
        self.replay_attempts.push(attempt);
        self.updated_at_ms = updated_at_ms;
    }

    /// Attach an explicit proof receipt to this queue entry.
    pub fn attach_receipt(&mut self, receipt: ProofIntentAttachedReceipt, updated_at_ms: i64) {
        self.attached_receipts.push(receipt);
        self.updated_at_ms = updated_at_ms;
    }
}

/// Result of an idempotent queue operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofIntentQueueMutation {
    /// Whether the queue file was changed.
    pub created: bool,
    /// Queue length after the operation.
    pub queue_len: usize,
    /// Existing or newly-created entry.
    pub entry: ProofIntentQueueEntry,
}

/// Errors returned by the durable proof-intent queue.
#[derive(Debug)]
pub enum ProofIntentQueueError {
    /// Filesystem I/O failed.
    Io(io::Error),
    /// JSON serialization failed.
    Json(serde_json::Error),
    /// A JSONL row failed to parse.
    ParseLine {
        /// One-based line number.
        line: usize,
        /// Parse error.
        source: serde_json::Error,
    },
    /// A parsed row violated queue invariants.
    InvalidEntry {
        /// Intent id, when available.
        intent_id: String,
        /// Human-readable invariant failure.
        reason: String,
    },
    /// The requested intent id was not present in the queue.
    MissingIntent {
        /// Missing intent id.
        intent_id: String,
    },
}

impl fmt::Display for ProofIntentQueueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "proof-intent queue I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "proof-intent queue JSON failed: {error}"),
            Self::ParseLine { line, source } => {
                write!(
                    formatter,
                    "proof-intent queue line {line} did not parse: {source}"
                )
            }
            Self::InvalidEntry { intent_id, reason } => {
                write!(
                    formatter,
                    "proof-intent queue entry {intent_id} is invalid: {reason}"
                )
            }
            Self::MissingIntent { intent_id } => {
                write!(formatter, "proof-intent queue has no entry {intent_id}")
            }
        }
    }
}

impl std::error::Error for ProofIntentQueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) | Self::ParseLine { source: error, .. } => Some(error),
            Self::InvalidEntry { .. } | Self::MissingIntent { .. } => None,
        }
    }
}

impl From<io::Error> for ProofIntentQueueError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ProofIntentQueueError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Read a durable proof-intent JSONL queue. Missing files are empty queues.
///
/// # Errors
/// Returns [`ProofIntentQueueError`] for I/O, JSON, or invariant failures.
pub fn load_proof_intent_queue(
    path: impl AsRef<Path>,
) -> Result<Vec<ProofIntentQueueEntry>, ProofIntentQueueError> {
    let path = path.as_ref();
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut entries: Vec<ProofIntentQueueEntry> = Vec::new();
    for (line_index, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: ProofIntentQueueEntry =
            serde_json::from_str(line).map_err(|source| ProofIntentQueueError::ParseLine {
                line: line_index + 1,
                source,
            })?;
        entry.validate()?;
        if let Some(position) = entries
            .iter()
            .position(|known| known.intent.intent_id == entry.intent.intent_id)
        {
            if let Some(slot) = entries.get_mut(position) {
                *slot = entry;
            }
        } else {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Rewrite a durable proof-intent JSONL queue after validating every row.
///
/// # Errors
/// Returns [`ProofIntentQueueError`] for I/O, JSON, or invariant failures.
pub fn write_proof_intent_queue(
    path: impl AsRef<Path>,
    entries: &[ProofIntentQueueEntry],
) -> Result<(), ProofIntentQueueError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut bytes = Vec::new();
    for entry in entries {
        entry.validate()?;
        serde_json::to_writer(&mut bytes, entry)?;
        bytes.push(b'\n');
    }
    fs::write(path, bytes)?;
    Ok(())
}

/// Idempotently queue a proof intent by its content-addressed id.
///
/// # Errors
/// Returns [`ProofIntentQueueError`] for load/write/validation failures.
pub fn queue_proof_intent(
    path: impl AsRef<Path>,
    entry: ProofIntentQueueEntry,
) -> Result<ProofIntentQueueMutation, ProofIntentQueueError> {
    entry.validate()?;
    let path = path.as_ref();
    let mut entries = load_proof_intent_queue(path)?;
    if let Some(existing) = entries
        .iter()
        .find(|known| known.intent.intent_id == entry.intent.intent_id)
        .cloned()
    {
        return Ok(ProofIntentQueueMutation {
            created: false,
            queue_len: entries.len(),
            entry: existing,
        });
    }

    entries.push(entry.clone());
    write_proof_intent_queue(path, &entries)?;
    Ok(ProofIntentQueueMutation {
        created: true,
        queue_len: entries.len(),
        entry,
    })
}

/// Update one queued proof intent in place.
///
/// # Errors
/// Returns [`ProofIntentQueueError`] for load/write/validation failures or a
/// missing intent id.
pub fn update_proof_intent_queue_entry(
    path: impl AsRef<Path>,
    intent_id: &str,
    update: impl FnOnce(&mut ProofIntentQueueEntry),
) -> Result<ProofIntentQueueEntry, ProofIntentQueueError> {
    let path = path.as_ref();
    let mut entries = load_proof_intent_queue(path)?;
    let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.intent.intent_id == intent_id)
    else {
        return Err(ProofIntentQueueError::MissingIntent {
            intent_id: intent_id.to_string(),
        });
    };
    update(entry);
    entry.validate()?;
    let updated = entry.clone();
    write_proof_intent_queue(path, &entries)?;
    Ok(updated)
}

fn canonical_value(value: &str) -> String {
    format!("{}:{value}", value.len())
}

fn canonical_field(label: &str, value: &str) -> String {
    format!("{label}={}", canonical_value(value))
}

fn canonical_option_field(label: &str, value: Option<&str>) -> String {
    match value {
        Some(value) => format!("{label}=some:{}", canonical_value(value)),
        None => format!("{label}=none"),
    }
}

/// Local SHA-256 hex helper. Kept module-local (matching the per-module pattern
/// used across the crate, e.g. `plan.rs`/`approval.rs`) so this foundational
/// type does not couple to a higher-level module for its content hash.
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
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
    fn intent_id_disambiguates_delimiter_bearing_fields() {
        let a = ProofIntent::new(
            "cargo test;scope=package:beta",
            ProofScope::Package {
                package: "gamma".to_string(),
            },
            ProofKind::Test,
            "sha256:tree-aaaa",
            None,
            true,
            None,
            None,
            ProofRedactionPolicy::Standard,
            1_704_000_000_000,
        );
        let b = ProofIntent::new(
            "cargo test",
            ProofScope::Package {
                package: "beta;scope=package:gamma".to_string(),
            },
            ProofKind::Test,
            "sha256:tree-aaaa",
            None,
            true,
            None,
            None,
            ProofRedactionPolicy::Standard,
            1_704_000_000_000,
        );

        assert_ne!(a.canonical_string(), b.canonical_string());
        assert_ne!(a.intent_id, b.intent_id);
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
    fn serializes_to_golden_fixture() -> Result<(), serde_json::Error> {
        let p = sample("sha256:tree-aaaa", 1_704_000_000_000);
        let json = serde_json::to_string(&p)?;

        assert_eq!(
            json,
            concat!(
                r#"{"schema_version":1,"intent_id":"proof:4be038ac8e943af059823fb25c8d690c","#,
                r#""command":"RCH_REQUIRE_REMOTE=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/x cargo test -p frankenterm-core --lib steering","#,
                r#""scope":{"type":"package","package":"frankenterm-core"},"kind":"test","#,
                r#""source_hash":"sha256:tree-aaaa","#,
                r#""expected_artifact_path":"target/test-logs/steering/result.jsonl","#,
                r#""required_remote":true,"bead_id":"ft-7h5da.6.1","attestation_slot":null,"#,
                r#""redaction_policy":"standard","created_at_ms":1704000000000}"#
            )
        );
        Ok(())
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
        assert!(
            json.contains("\"kind\":\"test\""),
            "proof kind present: {json}"
        );
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
    fn validate_rejects_id_mismatch() {
        let mut p = sample("sha256:tree-aaaa", 1_704_000_000_000);
        p.intent_id = "proof:00000000000000000000000000000000".to_string();

        assert!(matches!(
            p.validate(),
            Err(ProofIntentError::IntentIdMismatch { .. })
        ));
    }

    #[test]
    fn redaction_policy_defaults_to_standard() {
        assert_eq!(
            ProofRedactionPolicy::default(),
            ProofRedactionPolicy::Standard
        );
    }

    fn sample_queue_entry(source_hash: &str) -> ProofIntentQueueEntry {
        ProofIntentQueueEntry::new(
            sample(source_hash, 1_704_000_000_000),
            vec![
                "rch".to_string(),
                "--no-self-healing".to_string(),
                "exec".to_string(),
                "--".to_string(),
                "env".to_string(),
                "CARGO_TARGET_DIR=/tmp/ft-w8-target".to_string(),
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "frankenterm-core".to_string(),
            ],
            vec![
                ProofIntentEnvVar {
                    name: "RCH_REQUIRE_REMOTE".to_string(),
                    value: "1".to_string(),
                },
                ProofIntentEnvVar {
                    name: "RCH_NO_SELF_HEALING".to_string(),
                    value: "1".to_string(),
                },
            ],
            Some("/tmp/ft-w8-target".to_string()),
            "wait_rch",
            1_704_000_000_000,
        )
    }

    #[test]
    fn queue_proof_intent_is_idempotent_by_content_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("proof-intents.jsonl");
        let entry = sample_queue_entry("sha256:tree-queue");

        let first = queue_proof_intent(&queue, entry.clone()).expect("first queue");
        let second = queue_proof_intent(&queue, entry).expect("second queue");
        let loaded = load_proof_intent_queue(&queue).expect("load queue");

        assert!(first.created);
        assert!(!second.created);
        assert_eq!(first.queue_len, 1);
        assert_eq!(second.queue_len, 1);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].intent.intent_id, first.entry.intent.intent_id);
    }

    #[test]
    fn queue_entry_tracks_staleness_from_live_source_hash() {
        let entry = sample_queue_entry("sha256:tree-old");

        assert!(!entry.is_stale("sha256:tree-old"));
        assert!(entry.is_stale("sha256:tree-new"));
    }

    #[test]
    fn queue_entry_records_replay_attempts_and_attached_receipts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = dir.path().join("proof-intents.jsonl");
        let entry = sample_queue_entry("sha256:tree-updates");
        let intent_id = entry.intent.intent_id.clone();
        queue_proof_intent(&queue, entry).expect("queue");

        let updated = update_proof_intent_queue_entry(&queue, &intent_id, |entry| {
            entry.record_replay_attempt(
                ProofIntentReplayAttemptRef {
                    attempt_record_path: Some("artifacts/proof.attempt.json".to_string()),
                    outcome: "blocked_local_fallback".to_string(),
                    remote_cargo_reached: false,
                    local_fallback_detected: true,
                    recorded_at_ms: 1_704_000_000_100,
                },
                1_704_000_000_100,
            );
            entry.attach_receipt(
                ProofIntentAttachedReceipt {
                    receipt_path: "artifacts/proof.attempt.json".to_string(),
                    attached_at_ms: 1_704_000_000_200,
                },
                1_704_000_000_200,
            );
        })
        .expect("update queue");

        assert_eq!(updated.replay_attempts.len(), 1);
        assert_eq!(updated.attached_receipts.len(), 1);
        assert_eq!(updated.updated_at_ms, 1_704_000_000_200);
    }
}
